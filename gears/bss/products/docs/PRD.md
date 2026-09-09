---
refs:
  - bss/manifest/vz-arch-manifest-bss-only.md
  - bss/prd/PRD-billing-ledger-balances-202604041200
  - bss/prd/PRD-billing-module-202601120119
  - bss/prd/PRD-billing-system-202601120119
  - bss/prd/PRD-contracts-agreements-202601120119
  - bss/prd/PRD-metering-pricing-module-202601120119
  - bss/prd/PRD-plan-price-modeling-202605281200
  - bss/prd/PRD-product-catalog-marketplace-202601120119
  - bss/prd/PRD-rating-engine-202604031200
  - bss/prd/PRD-subscriptions-entitlements-202601120119
  - bss/prd/PRD-subscriptions-lifecycle-202604021200
  - bss/prd/PRD-tariffs-pricing-logic-202604011200
---

Created:  2026-07-03 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# PRD — Product & SKU Management

> **Provenance (2026-07-16):** vendored from `constructorfabric/gears-rust` PR **#4177**
> (`add-product-sku-prd` @ `6d3aab4`, author Corw1n-of-Amber) — this branch is the canonical home
> (upstream detach decision, 2026-07-15/16); no back-port or drift-tracking obligation.
> **Local changes applied at vendoring:** gear renamed **`product-sku` → `products`** (2026-07-16,
> incl. the ID prefix `cpt-cf-bss-product-sku-*` → `cpt-cf-bss-products-*`), and the RG2 fix in `cpt-cf-bss-products-fr-metering-unit-declaration`
> (unit ≠ dimension; the separate-SKUs mandate for multi-dimension usage replaced with the
> plan-price-owned dimension-set model — rating `SEAMS.md` §I RG2). **Known localization debt
> (tracked as rating SEAMS §I RG3):** pre-consolidation names (`PRD-tariffs-pricing-logic`,
> `PRD-rating-engine`, actor `…-actor-tariffs` — post ADR-0002 both map to the one **rating** gear,
> `gears/bss/rating/docs/PRD.md`) and `refs` front-matter paths in the legacy `docs/bss/prd/…`
> layout (kept verbatim as provenance). **RG3 reconciled 2026-07-16** at the first substantive
> edit: actor `…-actor-tariffs` merged into `…-actor-rating`; §2.1 delegations and the §17
> reference table localized to `gears/bss/rating/docs/PRD.md` / `gears/bss/subscriptions/docs/PRD.md`.
> **Further local change (D-46, 2026-07-16):** the `sellable` flag FR (`fr-sku-sellable`) —
> offering eligibility, enforced as pricing sellability-gate predicate 6.

<!-- toc -->

- [1. Overview](#1-overview)
  - [1.1 Purpose](#11-purpose)
  - [1.2 Background / Problem Statement](#12-background--problem-statement)
  - [1.3 Goals (Business Outcomes)](#13-goals-business-outcomes)
  - [1.4 Glossary](#14-glossary)
- [2. Architecture Alignment](#2-architecture-alignment)
  - [2.1 Catalog Decomposition and Registry Boundary](#21-catalog-decomposition-and-registry-boundary)
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
  - [6.1 Identifiers & Integrity](#61-identifiers--integrity)
  - [6.2 Product & Taxonomy Definition](#62-product--taxonomy-definition)
  - [6.3 SKU Definition & Classification](#63-sku-definition--classification)
  - [6.4 Attributes & Localization](#64-attributes--localization)
  - [6.5 Versioning, Lifecycle & Deprecation](#65-versioning-lifecycle--deprecation)
  - [6.6 Catalog Versioning & Snapshots](#66-catalog-versioning--snapshots)
  - [6.7 Approval, Publishing & Eventing](#67-approval-publishing--eventing)
  - [6.8 Multi-Tenancy & Read Models](#68-multi-tenancy--read-models)
  - [6.9 Bulk Operations](#69-bulk-operations)
  - [6.10 Cloning](#610-cloning)
  - [6.11 Data Retention & Erasure](#611-data-retention--erasure)
  - [6.12 Cross-PRD Consistency](#612-cross-prd-consistency)
  - [6.13 Operational Resilience & Concurrency](#613-operational-resilience--concurrency)
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
  - [Identifiers & Integrity](#identifiers--integrity)
  - [Product & Taxonomy Definition](#product--taxonomy-definition)
  - [SKU Definition & Classification](#sku-definition--classification)
  - [Attributes & Localization](#attributes--localization)
  - [Versioning, Lifecycle & Deprecation](#versioning-lifecycle--deprecation)
  - [Catalog Versioning & Snapshots](#catalog-versioning--snapshots)
  - [Approval, Publishing & Eventing](#approval-publishing--eventing)
  - [Multi-Tenancy & Read Models](#multi-tenancy--read-models)
  - [Bulk Operations](#bulk-operations)
  - [Cloning](#cloning)
  - [Data Retention & Erasure](#data-retention--erasure)
  - [Cross-PRD Consistency](#cross-prd-consistency)
  - [Error & Negative Paths](#error--negative-paths)
  - [Operational Resilience & Concurrency](#operational-resilience--concurrency)
  - [Non-Functional Requirements (Show-Stoppers)](#non-functional-requirements-show-stoppers)
- [13. Dependencies](#13-dependencies)
- [14. Assumptions](#14-assumptions)
- [15. Open Questions](#15-open-questions)
- [16. Risks](#16-risks)
- [17. Reference Materials](#17-reference-materials)
  - [17.1 Configurable-Policy Interim Defaults](#171-configurable-policy-interim-defaults)
  - [17.2 Monetization-Model Traceability](#172-monetization-model-traceability)

<!-- /toc -->

## 1. Overview

### 1.1 Purpose

**Product & SKU Management** is the authoritative, multi-tenant **catalog registry** for VHP BSS: the System of Record for *what can be sold, how it is described, classified, versioned, and published*. It owns Products, SKUs, categories/taxonomy, attributes/localization, and immutable catalog versions, with **financial-grade governance** (approval-gated publishing, immutable audit, deterministic snapshots) so that Plan & Price Modeling, Subscriptions, Contracts, Tariffs/Rating, Billing, Marketplace, and Presentation build on **stable, versioned, reproducible** catalog references.

It owns the **registry** half of BSS manifest §4.1 and stops at the SKU (including the `bundle` type flag and the metering-unit declaration). All commercial-pricing concerns are delegated by reference to the sibling decomposition PRDs (§2.1).

### 1.2 Background / Problem Statement

BSS must monetize diverse offerings (IaaS, PaaS, SaaS, marketplace services) across a multi-tenant, brand/region-scoped hierarchy. Without a single authoritative catalog registry, plan/price authoring, subscriptions, contracts, rating, and billing bind to mutable, non-reproducible product state — breaking posted invoices, active contracts, and in-flight subscriptions when the catalog changes, and leaving governance (who approved what, when) unauditable.

This PRD carves the **registry** scope out of the combined predecessor (`PRD-product-catalog-marketplace-202601120119`), completing the §4.1 decomposition already begun by Tariffs and Plan & Price Modeling. It fixes lifecycle/versioning semantics (draft → published [↔ deprecated] → retired, immutable history), a catalog publish contract (approval-gated, idempotent, event-fanned-out), a catalog-wide immutable `CatalogVersion` snapshot, and a stable SKU contract (identity, type, `PlanTier`, metering-unit declaration) that downstream modules can assume without re-validation.

### 1.3 Goals (Business Outcomes)

- **Flexibility / time-to-market**: Product Managers self-serve Product, SKU, category, and attribute changes across offering types without engineering involvement.
- **Stable monetization foundation**: every published SKU exposes a stable identifier, type, `PlanTier` classification, and metering-unit declaration, so plan/price authoring and rating bind to a fixed reference.
- **Auditable governance**: two-person approval for material catalog changes, immutable version history, and a complete CloudEvents audit trail satisfy financial and regulatory controls.
- **Safe evolution**: backward-compatible schema evolution and immutable `CatalogVersion` snapshots let the catalog change without breaking posted invoices, active contracts, or in-flight subscriptions.
- **Single source of truth**: one authoritative registry feeds partner/brand/region-scoped offerings, marketplace listings, and contract quotes.

> **Note**: The registry-vs-commercial boundary is stated canonically in §2.1. Where requirements or acceptance criteria touch commercial concerns, they define only the **registry-side contract** and reference the owning PRD.

### 1.4 Glossary

| **Term** | **Definition** |
|----------|----------------|
| **Catalog (registry)** | The authoritative registry of products/services/bundles/SKUs, categories, and localized attributes, and the catalog-wide version/publish mechanism (manifest §4.1). SoR: BSS. Defines *what can be sold and how it is described, classified, and published* — not how it is priced. |
| **Product** | A sellable or describable offering record with a name, **one required primary category plus optional secondary categories**, lifecycle state, brand/region scope, and version. The top of the catalog hierarchy. Identified by a system-generated `productId`. |
| **SKU (Stock Keeping Unit)** | A uniquely identifiable variant of a Product, typed as `product`, `service`, or `bundle`, optionally carrying a **metering-unit declaration** (for usage products) and stable accounting codes (`taxCategory`, `glCode`). A SKU has two identifiers: a system-generated immutable `skuId` and an operator-supplied human-readable `skuCode`. A SKU carries its own brand/region scope, **contained within its parent Product's scope**; the SKU→Product link is immutable after first publish. |
| **Usage SKU** | Definition, not detection: a SKU that **carries a metering-unit declaration**. There is no separate "is-usage" flag — declaring a metering unit **is** what makes a SKU a usage SKU. "A usage SKU missing its declaration" is not a detectable registry state; usage-completeness is enforced at the plan-price seam, never at registry publish. |
| **Sellable** | Per-SKU offering-eligibility flag (`sellable`, default `true`; D-46). `sellable = false` = **composition/metering-only**: the SKU publishes normally, MAY be referenced as a bundle/plan component and MAY carry a metering-unit declaration, but MUST NOT be offered **standalone** (pricing sellability-gate predicate 6). Distinct from lifecycle (`published` = *referenceable*) and from per-market GA gates (`not_sellable_ga`). The migration cover for technical/component SKUs of existing catalogs. |
| **Identifier** | The registry distinguishes **system identity** from **human/business code**. `productId`/`skuId` are server-generated immutable UUIDs. `skuCode` is operator-supplied, fixed-format, tenant-unique, immutable after first publish. Products MAY carry an optional `productCode` under the same reservation rules. Downstream consumers bind to `skuId`; humans/external catalogs reference `skuCode`/`productCode`. |
| **Bundle (SKU type)** | A SKU whose `type = bundle`. This PRD owns only the **type flag and identity**; the bundle's commercial composition (included SKUs, constraints, revenue share, invoice itemization) is authored in plan-price. A published bundle is commercially incomplete until composed. |
| **Category** | A node in the catalog taxonomy for browse, search, curation, and marketplace listing classification; supports hierarchy. |
| **Attribute** | A **governed** (defined, typed, optionally localized) key/value descriptor attached to a Product or SKU with brand/region visibility, managed via attribute **definitions**. Contrast the ungoverned **Metadata map**. |
| **Metadata map** | An **ungoverned**, per-entity free-form key/value channel for machine metadata (external ids, sync markers, migration tags): tenant-scoped, size-bounded, non-localized, excluded from read-model search, still PII-prohibited, captured in `CatalogVersion` snapshots. |
| **Brand** | A commercial/presentation identity within a tenant under which Products/SKUs/attributes are scoped for visibility, isolation, and localized display. A **visibility/legal scope, not a pricing dimension**. |
| **Region** | A geography/jurisdiction scope on Product/SKU/Attribute governing **visibility, legal availability, and localization fallback** — **never** pricing (currency/price-region/FX are plan-price/Tariffs). Drives name-uniqueness and read-model scoping. The region-set algebra is a pre-approval gate (§15). |
| **PlanTier** | Mandatory classification carried on SKUs/Plans (manifest §4.1) consumed by Subscriptions, `SlaPolicy`, and quota/entitlement policies. This PRD owns the **PlanTier taxonomy and the SKU-level value**; plan-price enforces presence at **plan** publish. Distinct from **OrgTier** (a partner commercial standing that never changes tenant topology). |
| **Metering-unit declaration** | The unit identity (e.g. vCPU-hours, GB-storage) declared on a usage SKU. This PRD owns the **declaration and its validation**; usage collection is OSS metering, plan-level meter binding is plan-price, and rating is Rating. |
| **Lifecycle state** | The Product/SKU state machine: `draft → published [↔ deprecated] → retired`, plus `draft → discarded` for never-published entities. `deprecated` is a governed sub-state of `published` (referenceable by existing consumers, closed to new adoption). `retired` is terminal (revival only via clone). `discarded` is terminal for an abandoned never-published draft (releases the `skuCode` reservation, audited, emits a discard event). |
| **Deprecation** | A governed marking of a `published` SKU that blocks **new adoption** while existing references continue, ahead of eventual retirement; modeled as the `deprecated` sub-state. |
| **Retirement / EOL** | Retirement runs as a **scheduled transition**: at initiation the entity is forced into `deprecated` (new adoption blocked immediately, still browsable) for the lead-time window (≥ 30 days interim), then flips to `retired` at `effectiveAt`. **EOL** is the optional stronger variant that additionally sets a `mustMigrateBy` date; the registry guarantees only the event + lead-time contract, while live-subscription migration is owned by subscriptions-lifecycle. |
| **Revision vs published version** | Two counters. The **internal revision** increments on every save (incl. draft edits) for optimistic concurrency and audit. The **published version** increments only on publish and is what downstream consumers and `CatalogVersion` reference. Draft churn MUST NOT inflate the published version. |
| **CatalogVersion** | An immutable, checksummed, **full** published snapshot of the catalog at publish time (monotonic `catalogVersionId`), enumerating the published Product/SKU/Category/Attribute set and their published versions. Re-resolving a `catalogVersionId` MUST always yield a byte-identical checksum. plan-price/Contracts/Billing freeze their own content keyed to `catalogVersionId`. One **component** of a downstream `pricingSnapshotRef` (defined in Tariffs); not equal to it. |
| **Material change / Materiality threshold** | A change is material when it touches the enumerated material-field set (canonically defined in the Materiality-gated-publish FR/AC) or exceeds a configured count of affected entities. The threshold is a typed, configurable policy with a default; material changes trigger the two-person rule. |
| **Recognized-unit set** | The configured set of metering units a usage SKU may declare. Owner and add-unit approval path are an owned dependency (§15); the registry validates declarations against the configured set. |
| **Two-person rule** | A multi-approver control requiring at least two distinct approvers (each distinct from the author) for material/above-threshold catalog changes before publication. |
| **Read model** | Cache-first, query-optimized projection of published catalog content for high-throughput browse/search; converges within a bounded window of the write model. |
| **Active-reference count** | Whether a SKU has live downstream references (plans, subscriptions, contracts). Derived as a **3-state predicate** from the `SkuReferenceCount` signal: **fresh-zero ⇒ unreferenced** (enables correction), **fresh > 0 ⇒ referenced**, **stale/never-received ⇒ conservatively referenced**. |
| **`SkuReferenceCount` signal** | A named, owned integration contract by which downstream modules (Subscriptions, Contracts, plan-price) publish reference liveness via a **per-producer watermark** ("as of `T`, my complete live-reference set is {…}"). Absence of a `skuId` under a fresh watermark ⇒ zero for that producer. `referenced` is a boolean OR across producers; the registry never sums across producers. A stale watermark is conservatively referenced + alerted; never-received is conservative but flagged distinctly. |
| **freezeComplete** | A per-`CatalogVersion` flag indicating that all registered freeze-participants (plan-price, Contracts, Billing) have acknowledged freezing their referenced content for that `catalogVersionId`. Resolution for posted/contractual use is rejected until `freezeComplete`, with a bounded timeout that fails closed. |
| **Freeze-participant set** | The configured list of modules that MUST acknowledge a freeze for a `CatalogVersion`. Membership is governed (two-person) and snapshotted into each `catalogVersionId`. |
| **Grandfathering** | Continuation of an existing reference on its frozen snapshot after the underlying SKU is deprecated/retired. The registry guarantees the snapshot is never mutated; eligibility policy is owned by plan-price / subscriptions-lifecycle. |
| **`compositionPending`** | A registry flag on a `bundle` SKU published-with-override while still uncomposed. While `true` the SKU is not-yet-adoptable for new references; cleared by a plan-price composition signal and emitted as `BundleCompositionCompleted`. |
| **OrgTier** | A partner's commercial standing (a partner commercial projection that MUST NOT change tenant topology). Distinct from `PlanTier`. |

## 2. Architecture Alignment

| **Field** | **Value** |
|-----------|----------|
| **Applicable Manifest(s)** | BSS |
| **Manifest Chapters** | §4.1 Product and Service Catalog (primary — catalog **registry**: Product, SKU, Category, Attribute, CatalogVersion, lifecycle, approvals, publish); §4.4 Billing and Invoicing (posting/snapshot immutability); §4.3 Subscriptions, §4.2 Rating, §4.6 Contracts, §4.8 Marketplace (consumers of published SKUs); §2.1.3 Multi-tenant semantics; §7.2 Event governance (CloudEvents 1.0) |

> **Normative alignment**: This PRD owns the **catalog registry** half of BSS §4.1 — the SoR for **Product, SKU, Category, Attribute, and CatalogVersion**, plus their authoring, versioning, lifecycle, taxonomy, localization, governance, publishing, and catalog-wide snapshotting. It MUST NOT contradict the sibling decomposition PRDs and MUST delegate, by reference, every commercial-pricing concern.

### 2.1 Catalog Decomposition and Registry Boundary

The combined Catalog (§4.1) capability is split across complementary PRDs (registry **plus** plan-price together realize "Catalog"). This PRD implements the **registry** half and is authoritative ONLY for the registry concerns above. The following are owned elsewhere and MUST NOT be re-specified here:

- **Plan, Price, PriceWindow, PriceList, Bundle composition, add-on rules, billing descriptors (invoice line template / tax category / GL code), plan lifecycle/migration/grandfathering, plan publish validation & approval, price history** → `PRD-plan-price-modeling-202605281200`.
- **Price resolution / override precedence / tier-volume-hybrid-commitment math / FX / `pricingSnapshotRef` composition** → the **rating** gear (evaluation core), `gears/bss/rating/docs/PRD.md`.
- **Usage → RatedCharge → BillableItem** → the **rating** gear (pipeline), `gears/bss/rating/docs/PRD.md`. **Subscription lifecycle** → `gears/bss/subscriptions/docs/PRD.md`. **Marketplace vendor ops (§4.8)** → `PRD-product-catalog-marketplace-202601120119`.

> **Registry vs commercial boundary (normative):** A **SKU** is the unit of *what exists and how it is described/classified/published*; a **Plan/Price** is the unit of *how it is sold and charged*. This PRD stops at the SKU (the `bundle` type flag and the metering-unit declaration). Two corollaries: **(a) No monetization-model marker** — the registry carries no monetization-model field; only **usage** leaves a footprint (the metering-unit declaration); absence of a model marker is intentional, not a gap. **(b) `region` is visibility/legal scope only** — never a pricing dimension here.

### 2.2 Predecessor PRDs and Scope Migration

- **PRD-product-catalog-marketplace-202601120119** (combined Catalog §4.1 + Marketplace §4.8) — the **registry** scope items ("Product & SKU Management", "Catalog Versioning", catalog-level approval workflows, "Localization & Branding Infrastructure", taxonomy/attributes) are **superseded by this PRD**; its `UC-product-sku-management-202601121200` use-case doc is superseded here. Marketplace (§4.8) scope remains authoritative there pending a dedicated Marketplace PRD.
- **PRD-plan-price-modeling-202605281200** — authoritative for Plan, Price, PriceWindow, PriceList, Bundle composition, add-ons, billing descriptors, plan lifecycle/migration, plan publish validation & approval. This PRD provides the published SKU/Category/Attribute foundation and `CatalogVersion` that plan-price builds on. The two MUST stay consistent on: SKU identity & `bundle` type, metering-unit declaration, `PlanTier` taxonomy, and `CatalogVersion`.
- The **rating** gear (`gears/bss/rating/docs/PRD.md`; post-ADR-0002 consolidation of the former tariffs-pricing-logic + rating-engine PRDs) — downstream consumer of published SKUs and `CatalogVersion`; owns evaluation and charging.

> **Recommendation on the combined PRD (§15):** Do **not** delete `PRD-product-catalog-marketplace-202601120119`. After this PRD + plan-price + Tariffs are approved, refactor it into a Marketplace-only PRD (§4.8). Until then, this PRD is authoritative for catalog-registry requirements only.

## 3. Actors

### 3.1 Human Actors

#### Product Manager

**ID**: `cpt-cf-bss-products-actor-product-manager`

**Role**: Self-serves Product, SKU, category, and attribute authoring across offering types.
**Needs**: Product/SKU editor, taxonomy manager, attribute/localization editor, metering-unit and `PlanTier` selection, clone/templating.

#### Catalog Admin

**ID**: `cpt-cf-bss-products-actor-catalog-admin`

**Role**: Governs the catalog: publishes `CatalogVersion`, runs bulk import/export, manages break-glass, force-completes stuck freezes, requests immutable-field corrections.
**Needs**: Approval/publish console, bulk-operations console, freeze monitoring & recovery, break-glass elevation.

#### Finance Reviewer

**ID**: `cpt-cf-bss-products-actor-finance-reviewer`

**Role**: Reviews and approves finance-material catalog changes (`taxCategory`, `glCode`, `PlanTier`); second approver under the two-person rule for finance-bearing changes.
**Needs**: Pending-approval queue with diffs, pre-publish lint report, separation-of-duties enforcement.

#### Auditor

**ID**: `cpt-cf-bss-products-actor-auditor`

**Role**: Inspects immutable version history, audit trail, and lineage; exports for compliance.
**Needs**: Version timeline with diffs, tenant-scoped audit retrieval, break-glass audit-export.

#### Platform Owner

**ID**: `cpt-cf-bss-products-actor-platform-owner`

**Role**: Privileged cross-tenant operator; accesses foreign-tenant catalog only under time-boxed break-glass.
**Needs**: Break-glass read/audit-export (writes separately gated or disallowed in v1).

### 3.2 System Actors

#### Plan & Price Modeling

**ID**: `cpt-cf-bss-products-actor-plan-price`

**Role**: Consumes published SKU identity/type, metering-unit declaration, `PlanTier`, `CatalogVersion`; produces `SkuReferenceCount`, `freezeComplete` ack, and the bundle composition-completed signal that clears `compositionPending`.

#### Rating (evaluation core + pipeline)

**ID**: `cpt-cf-bss-products-actor-rating`

**Role**: The one **rating** gear (post ADR-0002 consolidation; absorbs the former "Tariffs / Pricing Logic" consumer — id `…-actor-tariffs` retired): consumes published SKU refs + `CatalogVersion` for price resolution, and the metering-unit declaration to map usage. No authoring here.

#### OSS Metering

**ID**: `cpt-cf-bss-products-actor-oss-metering`

**Role**: Emits usage values (external); consumes the metering-unit **declaration** on usage SKUs.

#### Subscriptions

**ID**: `cpt-cf-bss-products-actor-subscriptions`

**Role**: Consumes published SKU refs + `PlanTier` + `replacedBy` for eligibility/composition/migration; produces `SkuReferenceCount`; consumes `mustMigrateBy` (post-v1 EOL). Owns live-subscription migration.

#### Contracts & Agreements

**ID**: `cpt-cf-bss-products-actor-contracts`

**Role**: Consumes `CatalogVersion` snapshots for quotes; produces `SkuReferenceCount` (incl. draft/quote refs per producer contract) and `freezeComplete` ack.

#### Billing & Invoicing

**ID**: `cpt-cf-bss-products-actor-billing`

**Role**: Consumes published SKU refs + `CatalogVersion`; produces `freezeComplete` ack (descriptor freeze). Billing descriptors are authored in plan-price and frozen into `CatalogVersion`.

#### Marketplace & Vendor Portal

**ID**: `cpt-cf-bss-products-actor-marketplace`

**Role**: References published SKUs in vendor listings. Vendor ops remain in the Marketplace PRD (§4.8).

#### Presentation / Portals

**ID**: `cpt-cf-bss-products-actor-presentation`

**Role**: Consumes catalog read models for browse/search cache warming.

#### Events & Audit (Common Core)

**ID**: `cpt-cf-bss-products-actor-events-audit`

**Role**: Provides the shared event system: durable acceptance, per-consumer delivery/dead-letter state, bounded-backoff retry. Transport mechanics owned there.

#### Tenant Identity (OSS/AMS + IdP)

**ID**: `cpt-cf-bss-products-actor-oss-ams-idp`

**Role**: Supplies `tenantId`, brand/region claims, OrgTier projection targets, and role claims. The registry MUST NOT mutate tenant topology.

## 4. Operational Concept & Environment

### 4.1 Module-Specific Environment Constraints

- **Registry is upstream of all commercial modeling**: a SKU MUST be published before a Plan/Price can reference it. The registry MUST NOT require any downstream consumer to re-interpret mutable catalog state for **posted** periods; the `CatalogVersion` snapshot contract is authoritative (manifest §4.4).
- **Multi-tenant isolation**: tenant/brand/region scoping via IdP claims; deny-by-default at the gateway; cross-tenant access audited; time-boxed break-glass for platform-owner access.
- **`region` is visibility/legal scope, never pricing**; currency/price-region/FX live in plan-price/Tariffs.
- **Time**: scheduled publish (`publishAt`) and scheduled retirement (`effectiveAt`) are UTC; retirement lead-time ≥ 30 days (interim).
- **Eventing**: every state-changing mutation emits CloudEvents 1.0 onto the shared event system (Common Core) with `dataschema`+semver, correlation/causation, and per-aggregate ordering keys `(tenant, aggregate)`; **pseudonymous actor references only** (no direct PII). Delivery/ordering/dead-letter mechanics are owned by the common event system, not re-specified here.
- **Snapshots are financial records**: `CatalogVersion` snapshots + version history require a durability class (interim ≥ 11 nines / replicated storage), backup/restore with periodic checksum verification, and a cross-region/DR posture.

## 5. Scope

### 5.1 In Scope

| **Feature** | **Priority** | **Notes** |
|-------------|--------------|-----------|
| Product definition | `p1` | Create/update Products: name, one required primary category + optional secondary, description, brand/region scope, lifecycle, version (§4.1 Product). |
| Category & taxonomy | `p1` | Hierarchical Category tree; cycle-free; uniqueness within parent. |
| SKU definition & typing | `p1` | Define SKUs typed `product`/`service`/`bundle`; stable accounting codes; `bundle` type flag only (composition is plan-price). |
| Metering-unit declaration | `p1` | Declare/validate the usage metering unit (unit identity only); governed de-listing. Consumed by plan-price, metering, Rating. |
| PlanTier taxonomy & SKU classification | `p1` | Own the `PlanTier` taxonomy and the SKU-level value; plan-price enforces presence at plan publish. Distinct from OrgTier. |
| Attribute management & localization (i18n) | `p1` | Extensible attribute schema; i18n with brand/region visibility and fallback `(locale,region,brand) → (locale,brand) → (default-locale,brand) → global`. |
| Identifiers & integrity | `p1` | Server-generated immutable `productId`/`skuId`; operator `skuCode` fixed-format, tenant-unique, immutable after first publish; field-mutability rules. |
| Product/SKU versioning & immutable history | `p1` | Internal revision per save; published version on publish; immutable history with diff; optimistic concurrency. |
| Product/SKU lifecycle, deprecation & retirement | `p1` | `draft → published [↔ deprecated] → retired` state machine; scheduled publish (`publishAt`) + scheduled retirement (`effectiveAt`); parent-child publish-ordering + cascade; retirement/EOL blocks new adoption, preserves references/snapshots, emits consumer handoff + lead-time. |
| Catalog versioning & snapshots (CatalogVersion) | `p1` | Stage/publish immutable full `CatalogVersion` with checksum + monotonic id; byte-identical re-resolution; emit `CatalogVersionPublished`; expose `freezeComplete`. |
| Catalog approval & publishing workflow | `p1` | Approval-gated publish with two-person rule above a typed materiality policy; approvals pinned to the approved revision; idempotent mutation boundary. Categories & attribute definitions are governed live entities. |
| Multi-tenant isolation & brand/region scoping | `p1` | Tenant/brand/region scoping via IdP claims; deny-by-default; audited cross-tenant; time-boxed break-glass; RBAC. |
| Eventing, audit & integration surface | `p1` | Publish CloudEvents 1.0 for every state-changing mutation with `dataschema`+semver + correlation/causation + ordering keys; pseudonymous actors; replay/bootstrap; immutable audit. |
| Data retention & right-to-erasure | `p1` | Defined retention for retired entities/versions/audit; reconcile immutable audit + version history with GDPR/CCPA (pseudonymize operator PII, retain financial records). |
| Catalog read models (core browse/search) | `p1` | Cache-first read models scoped by tenant/brand/region, bounded convergence; premise of the show-stopper NFRs (§7). |
| Bulk import (catalog onboarding at scale) | `p1` | Bulk create/update/import with idempotency + per-row partial-failure; imports land in draft and pass the gated publish. Required at ≥ 10K-SKU scale. |
| Advanced search, filter & faceting | `p2` | Rich faceted search/filter over the core read model. |
| Catalog lint / validation & snapshot export | `p2` | `validate(lint)` before publish (warn at SKU publish, blocking-with-override at `CatalogVersion` publish for uncomposed bundles); `export snapshot`. |
| Bulk export & bulk lifecycle tooling | `p2` | Deterministic snapshot export; bulk lifecycle (mass deprecate/retire) beyond parent→child cascade. |
| Product/SKU cloning / templating | `p3` | Clone to a new draft with new identifiers; explicit copy/reset field disposition; pricing not copied. |

### 5.2 Out of Scope

- **API schemas, storage DDL/data models, error-code taxonomies** — Design document(s).
- **Plan, Price, PriceWindow, PriceList, Bundle composition, add-on rules, billing descriptors, plan lifecycle/migration/grandfathering, plan publish validation & approval, price history, bulk price import, effective-price preview** — plan-price. This PRD owns only the SKU `bundle` **type flag** and stable SKU accounting codes, not their commercial use.
- **Price resolution, override precedence, tier/volume/hybrid/commitment math, FX, `pricingSnapshotRef` composition** — Tariffs.
- **Usage collection/normalization and charge rating** — OSS metering + metering-pricing-module + Rating.
- **Subscription lifecycle, entitlement enforcement, proration, recurring charge generation** — subscriptions-lifecycle + subscriptions-entitlements.
- **Contract negotiation, customer-specific overrides, committed-usage true-up, SLA penalties** — contracts-agreements.
- **Tax determination/statutory invoicing and revenue recognition** — Tax Engine / Billing / Finance. Catalog supplies only the tax-category/GL **code** on the SKU.
- **Marketplace vendor onboarding/certification, listings, fee schedules, payouts, fraud holds** — Marketplace PRD (§4.8).
- **Customer-facing storefront UI** — Presentation layer; Catalog provides read-model APIs.
- **Promotional/coupon pricing, eligibility, lifecycle** — Promotions (TBD) + plan-price. A *sellable* promo/$0/"Free" offering is a **normal registry SKU** here (identity, type, `PlanTier`, visibility); only its $0/promotional **price** is out of scope. Registry rules apply identically; no separate promo entity.
- **Tenant merge/split and brand transfer between tenants** — explicit non-goal for v1.
- **Binary / media assets** (images, icons, datasheets) — not stored here in v1; the registry MAY carry asset reference URIs as attribute values.
- **Recurring availability windows and scheduled `CatalogVersion` publish** — out of scope for v1 (entity-level scheduled publish IS in scope; `CatalogVersion` publish stays manual so the freeze protocol is untouched).
- **Configurable products / CPQ** — not modeled; the registry uses a concrete SKU per variant.
- **Product-to-product relationships beyond `bundle`, parent-child, and `replacedBy`/supersedes** — not modeled in v1; a governed catalog-relationship block is a post-v1 consideration (§15).

## 6. Functional Requirements

> **Content boundary**: FRs define WHAT the registry must do, not schemas. Any concrete field/flag/event/idempotency-key name or format is an illustrative handle; canonical field schemas, formats/regexes, error codes, event catalog, and payloads are owned by the gear's DESIGN (`gears/bss/products/docs/DESIGN.md`, pending). Full Given/When/Then acceptance detail is preserved in §12; interim configurable-policy defaults are in §17.1.

### 6.1 Identifiers & Integrity

#### Identifier contract

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-identifier-contract`

`productId` and `skuId` **MUST** be server-generated immutable identifiers (never operator-supplied). `skuCode` **MUST** be operator-supplied, fixed-format, tenant-unique, reserved **atomically at create time**, and immutable after first publish; once first published a `skuCode` **MUST** be permanently reserved within the tenant and **MUST NOT** be reissued. Downstream consumers **MUST** bind to `skuId`. Products **MAY** carry an optional `productCode` under the same rules.

**Rationale**: Stable system identity plus a protected human/external code is the foundation every downstream reference depends on.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Product/SKU field-mutability matrix

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-field-mutability-matrix`

Mutability **MUST** be classified by lifecycle state: in `draft` all fields editable (incl. SKU→parent link and `skuCode`/`productCode`); after publish four buckets apply — **(i) structural identity** immutable and never correctable in place (remedied only by retire + clone); **(ii)** `type` and metering-unit declaration immutable but correctable via the governed fresh-zero path; **(iii) material-but-mutable** (`PlanTier`, `taxCategory`, `glCode`, `sellable`) change via a new published version under governance; **(iv)** other descriptive fields via a new published version. Illegal changes **MUST** be rejected fail-closed with an audited reason. The active-reference count **MUST** be sourced from `SkuReferenceCount` as the 3-state predicate; the registry **MUST NEVER** treat an entity as unreferenced absent a fresh watermark.

**Rationale**: Protecting identity/external caches while allowing governed evolution requires a per-state, per-field classification.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Reference-signal sourcing, freshness & counting

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-reference-signal`

The registry **MUST** consume the named `SkuReferenceCount` signal (per-producer watermark). Absence of a `skuId` under a **fresh** watermark ⇒ zero for that producer; `referenced` **MUST** be a boolean OR (any registered producer > 0); the registry **MUST NOT** sum across producers, and each producer dedups within itself. A **fresh-zero** across all registered producers ⇒ unreferenced; a **stale** or **never-received** watermark ⇒ conservatively referenced (stale alert distinct from never-received). Only **registered** producers count; Contracts **MUST** declare whether draft/quote references count, with identical semantics across mutability/correction/retirement.

**Rationale**: A watermark-based OR predicate scales to 10K+ SKUs × N producers without dense zero publishing and never falsely frees a referenced SKU.

**Actors**: `cpt-cf-bss-products-actor-plan-price`

#### Immutable-field correction (zero-reference & break-glass)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-immutable-field-correction`

For a correctable immutable field (`type` or metering-unit declaration; **never** structural identity), if `SkuReferenceCount` is **fresh and zero** across all producers the system **MUST** allow a governed re-publish under the two-person rule, increment the published version, and emit `SkuImmutableFieldCorrected`; absent a fresh-zero signal it **MUST** reject fail-closed. While the signal is entirely unavailable, a single-SKU correction **MAY** proceed only via **break-glass** (two-person + mandatory reason + `SkuCorrectionOverride` recording signal-unavailability), behind a feature flag OFF by default.

**Rationale**: Corrections must be provably safe (fresh-zero) or explicitly break-glass-audited, never silent.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

### 6.2 Product & Taxonomy Definition

#### Create a product

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-create-product`

Creating a Product **MUST** generate a `productId`, persist as `draft` (published version 0) with full multi-tenant isolation and an audit entry (primary category optional at draft, required at publish; optional secondary categories allowed). Name uniqueness **MUST** be enforced on `(tenantId, brandId, normalized(name))` within any overlapping region scope; two same-named Products are allowed only when region scopes are **disjoint**. Indeterminate region overlap **MUST** fail closed with an operator-facing reason. The uniqueness key is the **canonical internal name**; localized display name/description are well-known attributes.

**Rationale**: Multi-region catalogs need same-name coexistence without mangling, while same-region collisions must be rejected deterministically.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Manage taxonomy (create, rename, re-parent, retire, delete)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-manage-taxonomy`

Category operations **MUST** validate name uniqueness within parent (re-checked on rename/re-parent), reject cycles, and reject exceeding configured max depth/children. A Product carries **exactly one primary category + zero or more secondary**; the read model **MUST** make it filterable under every assigned category. Categories are **governed live entities** (in-place, two-person-gated on material ops, audited) — not draft/publish-versioned. Retire/delete **MUST** be blocked while any active Product references the category (as primary or secondary) or active child categories exist. Each operation emits the corresponding `Category*` event.

**Rationale**: Taxonomy reshaping affects browse for every published product, so it must be governed and cycle-free.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

### 6.3 SKU Definition & Classification

#### Define a SKU

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-define-sku`

Defining a SKU **MUST** link it to the Product, assign `skuId`/`skuCode`, type it `product`/`service`/`bundle`, and validate the per-type required-field set. A `bundle`-typed SKU **MUST** persist only the type flag and identity (composition authored in plan-price; blocking completeness enforced at `CatalogVersion` publish). Promotional/$0/"Free" SKUs **MUST** follow identical registry rules; there is no separate promo entity.

**Rationale**: A uniform, type-aware SKU contract lets downstream bind without re-validation.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Sellable flag (offering eligibility)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-sku-sellable`

A SKU **MUST** carry a dedicated `sellable` flag (default `true`), independent of lifecycle state (D-46, 2026-07-16). `sellable = false` marks a **composition/metering-only** SKU: it publishes normally, MAY be referenced as a bundle/plan component and MAY carry a metering-unit declaration, but **MUST NOT** be offerable standalone — plan-price enforces this as sellability-gate predicate **(6)** for standalone lines (bundle-**component** references are exempt; the component conjunction keeps predicates (1)–(5)). The flag is **material-but-mutable** (bucket iii of the mutability matrix): a change takes a new published version under governance and is frozen per `CatalogVersion`.

**Rationale**: `published` means *referenceable*, not *offerable* — migrated catalogs carry technical/component SKUs that must exist, meter, and compose without ever being sold alone; conflating the two forces either unpublishable components or accidentally offerable internals.

**Actors**: `cpt-cf-bss-products-actor-product-manager`, `cpt-cf-bss-products-actor-plan-price`

#### Declare metering unit

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-metering-unit-declaration`

Declaring a metering unit **MUST** validate it against the configured recognized-unit set and reject an unrecognized unit unless elevated approval marks a new validated unit. A usage SKU declares **exactly one** unit — the counted identity only. Pricing dimensions (`dimensionKey`) are **not** units: the declared dimension set over a meter is a plan-price concern (plan-level meter binding, §2.1), persisted on the plan-price revision and frozen into `pricingSnapshotRef` by Rating; multi-dimension usage on one unit **does not require separate SKUs**, and separate SKUs remain available where variants differ commercially (accounting codes, lifecycle). For a composite (derived) meter the SKU declares the composite's **output** unit; input units are referenced by the plan-price formula against the recognized-unit set and need no SKUs of their own. A draft whose unit was `deprecated` before first publish **MUST** be treated as a new declaration and rejected. The declared unit **MUST** be carried on publish; this PRD **MUST NOT** compute charges.

**Usage-source binding (UC3 seam adoption, 2026-07-28, flagged for veto)**: a metering-unit declaration **MUST** carry a **`usageTypeRef`** — the usage-collector `gts_id` of the UsageType whose accepted entries feed this meter — as the authoritative attribution binding (rating SEAMS **UC3**(a); the phase-2 usage-event feed the **rating PRD** commits to as the UC1 remedy — not yet authored in the usage-collector design set). Publish **MUST** validate that the referenced UsageType exists and is active, and that the meter's **declared dimension set equals the referenced UsageType's declared `metadata_fields` keys** (UC3(c) cross-validation — a mismatch means emitted usage carries dimensions the catalog cannot attribute, or the catalog prices dimensions the source never emits); either failure blocks publish with the offending keys named. Rating never guesses the binding: an unresolvable `usageTypeRef` quarantines the usage record rather than mis-attributing it.

**Rationale**: Declaring the unit is what defines a usage SKU; validation prevents downstream rate corruption.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Metering-unit de-listing

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-metering-unit-delisting`

De-listing a metering unit **MUST** be rejected while ≥ 1 `published` SKU references it; the system **MUST** instead support marking it **deprecated** (no new declarations) with full removal only once unreferenced. A unit's identity/semantics are **immutable** (e.g. `GB-storage` MUST NOT be silently redefined to GiB); a correction is a new unit + deprecation of the old. De-listing/deprecation **MUST** be audited and **MUST NOT** mutate any frozen snapshot.

**Rationale**: Redefining a live unit corrupts every downstream rate; deprecate-then-remove protects existing references.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### PlanTier classification

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-plantier-classification`

Assigning `PlanTier` **MUST** validate against the taxonomy owned here and carry it on the published SKU. A `PlanTier` value has a **stable tier code** as identity; **rename affects the display label only**. Managing the taxonomy (add/rename/retire) is a governed (two-person), audited operation emitting `PlanTierUpdated`; a value **MUST NOT** be retired while any `published` SKU carries it (deprecate-then-retire). The taxonomy **MUST** be seeded with a neutral value. `PlanTier` **MUST NOT** be conflated with OrgTier; plan-publish presence enforcement is delegated to plan-price.

**Rationale**: Stable tier codes keep SLA/quota policies from rippling on rename; universal presence is a manifest mandate.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Stable accounting codes on SKU

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-accounting-codes`

Setting tax-category and GL codes **MUST** persist them as stable codes and **validate each against a configured recognized set** (owner = Finance, deprecate-then-remove governance). The codes are **required at publish for `product`/`service`-type SKUs**; a SKU published without a required code is unpostable and **MUST** be rejected. The system **MUST NOT** compute tax or post to GL (codes only).

**Rationale**: Validating codes at authoring prevents unpostable SKUs surfacing weeks later at ERP export.

**Actors**: `cpt-cf-bss-products-actor-finance-reviewer`

### 6.4 Attributes & Localization

#### Localized attributes, well-known display fields, and definition lifecycle

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-localized-attributes`

Adding i18n attribute **values** **MUST** validate against the attribute **definition**, require the default-locale value (rejected at publish if absent), and resolve locale via `(locale, region, brand) → (locale, brand) → (default-locale, brand) → global` (default-locale resolved per brand, falling back to tenant default). The registry **MUST** seed **well-known display attribute definitions** (localized display name/description for Product/SKU/Category). Managing attribute **definitions** is a governed live-entity operation emitting `AttributeDefinitionUpdated`; changes **MUST** be backward-compatible and follow a deprecate-then-remove lifecycle. The registry **MUST** provide an ungoverned, size-bounded, non-localized, search-excluded, PII-prohibited **metadata map** for machine metadata.

**Rationale**: Localized display without a second identity key, plus a governed/ungoverned split, keeps portals correct and integrations from flooding the definition registry.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

### 6.5 Versioning, Lifecycle & Deprecation

#### Revision vs published version

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-revision-vs-version`

The system **MUST** increment the **internal revision** on every save (rejecting stale-revision writes via optimistic concurrency) while the **published version** increments only on publish. Consumers and `CatalogVersion` **MUST** reference the published version; historical versions **MUST** be retained with a diff and **MUST NOT** be modifiable. **Version binding** **MUST** be explicit: a new reference binds to the latest published version at bind time; a frozen reference keeps its snapshot; a bound-but-not-yet-frozen reference re-resolves at freeze and **MUST** surface a version-change diff to the freezing module rather than silently swapping.

**Rationale**: Separating draft churn from published versions keeps downstream snapshots and quotes stable and auditable.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Lifecycle transitions & reversibility

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-lifecycle-transitions`

The state machine `draft → published [↔ deprecated] → retired` (+ `draft → discarded`) **MUST** allow only its defined transitions, treat `retired` and `discarded` as terminal (revival only via clone), and allow downstream referencing only for `published`/`deprecated`. Entity publish **MAY** be **scheduled** (`publishAt`, UTC) with approval pinned at scheduling time and re-validated fail-closed at activation. Publication of incomplete entities **MUST** be rejected. There is **no `unpublish`** and **no in-place rollback** — retraction/reversion is `deprecate`/`retire` + a new version (forward-only).

**Rationale**: A constrained, forward-only state machine keeps caches/snapshots/posted content consistent.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Parent-child (Product↔SKU) lifecycle integrity

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-parent-child-integrity`

A SKU **MUST NOT** reach `published` while its parent Product is not `published`, and the system **MUST NOT** orphan a `published` SKU under a `retired` Product. A SKU's brand/region scope **MUST** be contained within its parent's; a scope-narrowing Product publish **MUST** fail closed while any non-`retired` child would fall outside. Retiring a Product with non-`retired` SKUs **MUST** require confirmed **cascade-retire** (partial by design, recording `direct` vs `cascaded` provenance; EOL-requiring children left un-retired and listed; never-published children auto-`discarded`). When a partial cascade leaves children, the parent **MUST** remain non-`retired` and its deferred-retire intent tracked/queryable.

**Rationale**: Hierarchy and scope-containment invariants prevent orphaned or out-of-scope published content.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Deprecation (governed sub-state)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-deprecation`

Marking a `published` SKU `deprecated` **MUST** move it to the `deprecated` sub-state, mark it so consumers **block new adoption** while existing references continue, and emit `SkuDeprecated`. `deprecated` **MUST** be a tracked, queryable state (not a flag), recording provenance `direct` (vs `cascaded`). The registry marks and exposes; the consumer enforces the new-adoption block (CI-verified via the seam suite).

**Rationale**: A tracked sub-state makes the new-adoption guard testable and consistent with the composition-pending pattern.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Un-deprecation

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-undeprecation`

`deprecated → published` **MUST** be allowed under the two-person rule, re-open new adoption, and emit `SkuUndeprecated`. Un-deprecating a **Product** reverses **only `cascaded`** child deprecations and **MUST NOT** revive a child's `direct` deprecation. The transition **MUST** be audited; a `retired` entity **MUST NOT** be reversible.

**Rationale**: Provenance-aware reversal prevents accidentally reviving individually-deprecated children.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Retirement / EOL consumer handoff

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-retirement-eol`

Retiring a referenced SKU **MUST** require explicit confirmation with the active-reference count shown, then run as a **scheduled transition**: force `deprecated` at initiation (new adoption blocked immediately, still browsable), preserve snapshots, emit `SkuRetired`/`ProductRetired` with `{ skuId, fromVersion, reason, replacedBy?, mustMigrateBy?, effectiveAt }` honoring the ≥ 30-day lead-time, then flip to `retired` at `effectiveAt`. The registry is SoR for `replacedBy` (a successor `published` SKU). **Joint contract with plan-price (closed 2026-07-28, pricing D-47/AC #82):** the registry never flips a SKU to `retired` while the `SkuReferenceCount` predicate reads referenced (fresh > 0, or stale/never-received — conservatively referenced, fail-closed); plan-price's side is AC #82 — referencing plans are flagged on the deprecation signal and new adoption is blocked while in-flight subscribers keep their frozen snapshots (grandfathering). **v1 = plain retirement + grandfathering only; EOL-with-`mustMigrateBy` is a defined-but-deferred post-v1 follow-on** that MUST stay disabled until the consuming subscriptions-lifecycle AC exists and is referenced by number, and requires a consumer acknowledgment contract (lapsed ack ⇒ suspend fail-closed + `SkuEolSuspended`).

**Rationale**: A defined lead-time state and successor pointer let consumers migrate safely without undefined limbo.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

### 6.6 Catalog Versioning & Snapshots

> **Two publish layers.** A Product/SKU has its own **published version** that increments each time that entity is published. A **`CatalogVersion`** is a catalog-wide immutable snapshot; a SKU can be published yet not appear in any `CatalogVersion` until the next catalog publish. New references bind to the latest published entity version at bind time; posted/contractual references freeze to a `catalogVersionId`.

#### Publish an immutable catalog version

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-catalog-version-publish`

Publishing a `CatalogVersion` **MUST** persist a **full** snapshot (published Product/SKU set + their versions + current categories/attributes captured together), assign a monotonic `catalogVersionId`, generate a checksum, record `stagedAt`/`publishedAt`, capture the freeze-participant set, and make it immutable (storing references to plan-price/Contracts/Billing content, not that content). An uncomposed `bundle` SKU **MUST** require explicit two-person override (blocking-with-override) and be flagged `compositionPending = true`. It **MUST** emit `CatalogVersionPublished` and expose per-version `freezeComplete`. A published `CatalogVersion` **cannot be withdrawn or rolled back** (roll-forward N+1 only); snapshot boundary is the whole tenant (serialized). **Increment-trigger taxonomy + batching SLO (pricing D-47, ratified 2026-07-28 — the joint contract with plan-price's `PlanPublished` pending-ref model):** an **interactive** downstream publish request increments immediately (coalescing window <= 5s); **bulk** operations (mass repricing, migrations, bulk enrollments) coalesce into one version with a **5-minute hard max delay**; the delay from a pending publish request to `CatalogVersionPublished` is bounded by **p95 <= 60s / max 5 min**, and the registry stays the **sole** incrementer.

**Rationale**: A full, immutable, checksummed snapshot is the reproducibility anchor for posted invoices and contracts.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Snapshot reproducibility

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-snapshot-reproducibility`

Re-resolving a `catalogVersionId` at any future time **MUST** yield a byte-identical checksum and unchanged **registry** content (manifest §4.4). `CatalogVersion` **MUST** be exposable as **one component** of a downstream `pricingSnapshotRef` without asserting it equals the full snapshot; referenced-module content reproducibility is governed by the freeze protocol.

**Rationale**: Byte-identical re-resolution is required for posting immutability and dispute defensibility.

**Actors**: `cpt-cf-bss-products-actor-billing`

#### Cross-module snapshot freeze atomicity

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-freeze-atomicity`

The system **MUST** expose a `freezeComplete` flag per `catalogVersionId` and **reject resolution for posted/contractual use** until all registered freeze-participants acknowledge, with a bounded timeout that **fails closed**. Read-only browse **MAY** proceed during the freeze window. The resolution API **MUST** require the consumer to **declare intent** (`browse` vs `posted/contractual`) so a consumer cannot post against a not-yet-`freezeComplete` version by mislabeling its call (consumer-side obligation, CI-verified in the seam suite).

**Rationale**: Cross-module atomicity prevents posting against a partially-frozen snapshot.

**Actors**: `cpt-cf-bss-products-actor-plan-price`

#### Freeze recovery & force-completion

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-freeze-recovery`

For a `CatalogVersion` past the freeze timeout the system **MUST** identify each non-acknowledging participant, support an **idempotent re-trigger** of the fan-out, and support **force-completion** under the two-person rule that records each missing participant as explicitly **not-frozen** and emits `FreezeForceCompleted`. Force-completion **MUST NOT** mark missing content as frozen; the default is **pinned fail-closed** for that participant's content (auto-fallback is an off-by-default later enhancement).

**Rationale**: A stuck freeze must be recoverable without silently marking content frozen.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Freeze-participant set governance

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-freeze-participant-governance`

Freeze-participant membership changes **MUST** be governed (two-person), audited, and each `CatalogVersion` **MUST** snapshot the participant set at publish time so a historical version re-resolves `freezeComplete` against its original participants. A participant removed after publish **MUST NOT** retroactively flip that version's `freezeComplete`.

**Rationale**: Historical versions must re-resolve against the participants that existed at publish.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Grandfathering invariant

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-grandfathering-invariant`

The registry **MUST** guarantee a grandfathered frozen snapshot is **never mutated**; retirement/deprecation affects only new adoption, never existing frozen references. Grandfathering **eligibility policy** is owned by plan-price / subscriptions-lifecycle; this requirement makes the delegation auditable from the registry side.

**Rationale**: Existing terms must persist byte-identically after the underlying SKU is deprecated/retired.

**Actors**: `cpt-cf-bss-products-actor-subscriptions`

#### Uncomposed-bundle adoption guard

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-bundle-adoption-guard`

A `bundle` SKU published with the uncomposed override **MUST** carry `compositionPending = true` until plan-price composes it, and consumers **MUST** treat `compositionPending` SKUs as **not-yet-adoptable** for new references. Clearing it **MUST** be driven by a plan-price composition signal, audited, and emitted as `BundleCompositionCompleted` (producing a new published version, never mutating a prior frozen `CatalogVersion`).

**Rationale**: An incomplete bundle must be reproducible-as-pending and blocked from new adoption until composed.

**Actors**: `cpt-cf-bss-products-actor-plan-price`

### 6.7 Approval, Publishing & Eventing

#### Materiality-gated publish

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-materiality-gated-publish`

A **material** change (touching `PlanTier`/metering-unit/`taxCategory`/`glCode`, a lifecycle transition to `published`/`deprecated`/`retired`, a Category create/rename/re-parent/retire/delete, a material attribute-definition change, or exceeding the configured affected-entity count) **MUST** enforce a two-person rule: ≥ two distinct approvers, each distinct from the author and holding CatalogAdmin or FinanceReviewer; a **finance-material** field (`taxCategory`, `glCode`, `PlanTier`) **MUST** include ≥ 1 FinanceReviewer. An approval **MUST** be **pinned to the internal revision**; any subsequent edit invalidates it and re-queues with the diff re-presented. The materiality rule **MUST** be a typed, configurable policy with an enforceable interim default (§17.1); a rejection returns the entity to `draft` with reason recorded.

**Rationale**: Two-person control with separation of duties and revision-pinning prevents unauthorized or bypassed publishes.

**Actors**: `cpt-cf-bss-products-actor-finance-reviewer`

#### Idempotent authoring boundary

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-idempotent-authoring`

For a retried create/update/publish with an idempotency key, the same key + identical payload **MUST NOT** create duplicate entities, versions, or events; the key **MUST** be scoped per tenant + endpoint + client key and retained ≥ 24h **and never less than the maximum freeze timeout**. Reuse with a **different** payload **MUST** be rejected as a conflict (no silent no-op).

**Rationale**: Idempotency prevents duplicate publishes on retry, including after the freeze window.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Registry eventing & audit

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-registry-eventing-audit`

Every state-changing mutation **MUST** publish the corresponding CloudEvents 1.0 event onto the shared event system, stamping correlation/causation + idempotency key + per-aggregate ordering keys `(tenant, aggregate)`; delivery/ordering/durability are owned by the common event system. Every state-changing requirement **MUST** map to exactly one named event (or an explicit "no event" decision in Design). Event payloads **MUST** carry **pseudonymous actor references only** (never direct operator PII). The mutation **MUST** be recorded in an immutable, queryable audit trail. Plan/Price/Bundle-composition events **MUST NOT** be emitted here (owned by plan-price).

**Rationale**: Complete, pseudonymous, ordered eventing + immutable audit is what makes erasure (AC #35) and downstream consumption work.

**Actors**: `cpt-cf-bss-products-actor-events-audit`

#### Event schema versioning & replay

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-event-versioning-replay`

Every event **MUST** carry a `dataschema` URI with a semantic version; a consumer pinned to `vN` **MUST** deserialize `vN+1` (new fields optional with defaults); out-of-order/duplicate delivery beyond the idempotency window **MUST** be detectable via `(tenant, aggregate, sequence)`. The system **MUST** provide a **bootstrap path** (latest `CatalogVersion` + event tail) for published-scope consumers, and **MUST** detect when a consumer checkpoint predates the available event tail and **fail loudly**.

**Rationale**: Forward-compatible schemas + a bootstrap path let consumers evolve and recover without full historical replay.

**Actors**: `cpt-cf-bss-products-actor-events-audit`

### 6.8 Multi-Tenancy & Read Models

#### Tenant/brand/region isolation & break-glass

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-tenant-isolation-breakglass`

Cross-scope query/mutation **MUST** be denied by default at the API gateway and the attempt audited. Privileged platform-owner cross-tenant access **MUST** use **break-glass** elevation that is time-boxed, reason-required, separately alertable, and itself two-person-approved or post-hoc-reviewed; standing cross-tenant access **MUST NOT** be granted.

**Rationale**: Cross-tenant catalog leakage is a critical commercial/competitive incident class.

**Actors**: `cpt-cf-bss-products-actor-platform-owner`

#### Break-glass action scope

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-breakglass-action-scope`

Break-glass **MUST** permit **read and audit-export only**; any write/publish under break-glass **MUST** be separately gated (two-person + distinct alert) or disallowed in v1. Every break-glass action **MUST** be individually audited with the elevation reason and correlation ID.

**Rationale**: Elevation must not silently grant write authority in a foreign tenant.

**Actors**: `cpt-cf-bss-products-actor-platform-owner`

#### Cache-first browse/search with bounded convergence

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-cache-first-browse`

Browse/search/filter **MUST** be served from cache-first read models scoped to the caller's tenant/brand/region, converging within its own budget (interim p99 < 2 s after write commit). Stale reads during the window **MUST** be safe (never expose unpublished or cross-scope content) and **MUST** carry the `asOfCatalogVersion` staleness signal. The per-state visibility contract **MUST** hold: `published` browsable; `deprecated` browsable with a machine-readable flag and excludable by filter; `retired` excluded from default browse and retrievable only via explicit history query.

**Rationale**: The show-stopper read NFRs and Performance vector are premised on a cache-first read model with a safe staleness contract.

**Actors**: `cpt-cf-bss-products-actor-presentation`

### 6.9 Bulk Operations

#### Bulk import/export

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-bulk-import-export`

Bulk import/export **MUST** apply **per-row idempotency**, report per-row success/failure (no hidden partial failure), and never leave a partially-inconsistent published state. Dependent rows **MUST** apply two-phase (stage-all-then-commit) or dependency-ordered, never committing an orphan. Idempotency operates at two levels (batch key + per-row keys). A bulk operation **MUST** emit a coalesced `CatalogBulkOperationCompleted` (no event storm). **Bulk import lands entities in `draft`**; publication remains gated, approved against an **aggregated change report** (counts, per-type summary, sample, lint findings). Export **MUST** be deterministic for a given `catalogVersionId`.

**Rationale**: Onboarding/migration at ≥ 10K-SKU scale cannot be row-by-row and must stay consistent and governed.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

### 6.10 Cloning

#### Clone a product/SKU

- [ ] `p3` - **ID**: `cpt-cf-bss-products-fr-clone`

Cloning a source Product/SKU (draft, published, or **retired** — the sanctioned revival path) **MUST** create a new `draft` with new `productId`/`skuId` and a new `skuCode`/optional `productCode` (system-suggested, operator-overridable, atomically reserved), copying structure/attributes/scoping/category/`PlanTier`/metering-unit while resetting lifecycle and version counters and **never copying** pricing/plan content. The cloned metering unit, `PlanTier`, and category assignment **MUST** be re-validated against live registries; the clone **MUST** fail or force re-selection if any was de-listed/deprecated/retired. It **MUST** record a `clonedFrom` reference and **MUST NOT** affect the source.

**Rationale**: Cloning accelerates catalog expansion and is the safe revival path for retired items, provided re-validation runs.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

### 6.11 Data Retention & Erasure

#### Retention & right-to-erasure

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-retention-erasure`

The system **MUST** retain financial/version/audit records for the configured retention duration and satisfy erasure of **actor PII** by **pseudonymizing** it across audit, entity version fields, and the actor identity-reference map — never deleting immutable financial/version records; because events carry only pseudonymous actor references, updating the reference map completes erasure without touching immutable event streams. Attribute/description free-text **MUST NOT** contain personal data — enforced by a **validation block at write** (hard prohibition, no erasure carve-out, fail-closed on uncertainty, curated allow-list for legitimate person-named products). Erasure **MUST NOT** break `CatalogVersion` reproducibility or audit completeness.

**Rationale**: Content-erasure is logically incompatible with byte-identical reproducibility, so PII is kept out at write and actor PII is pseudonymized.

**Actors**: `cpt-cf-bss-products-actor-auditor`

### 6.12 Cross-PRD Consistency

#### Registry ↔ plan-price seam

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-plan-price-seam`

There **MUST** be a shared schema-version pin and a CI contract test that fails when registry and plan-price diverge on any shared field (`skuId`, `bundle` type, metering-unit declaration, `PlanTier`, `CatalogVersion`); a runtime divergence **MUST** fail closed (reject the dependent plan publish). The same suite **MUST** assert consumer-side lifecycle obligations: reject adoption of `compositionPending`/`deprecated` SKUs; reject a usage binding when the target SKU has no declared unit (and reject/warn when its unit is `deprecated`) — **this is where usage-completeness is enforced**; consume `mustMigrateBy` (post-v1); resolve grandfathered refs against the frozen snapshot; re-validate on `SkuImmutableFieldCorrected`; and declare intent before `freezeComplete` on posted/contractual resolution. Assertions are authorable only once the referenced counterpart AC exists.

**Rationale**: A CI-verified seam turns delegated boundaries into enforced contracts rather than assumptions.

**Actors**: `cpt-cf-bss-products-actor-plan-price`

#### Monetization-model traceability

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-monetization-traceability`

The PRD **MUST** expose a traceability map (§17.2) so the registry's deliberate lack of a monetization-model marker does not read as an unmet requirement: flat/per-seat/tiered/volume/hybrid/commitment → authored/evaluated in plan-price + Tariffs; usage → metering-unit declaration here + binding/rating downstream. Absence of a model marker on a SKU **MUST** be treated as intentional, not a missing field.

**Rationale**: Explicit traceability prevents the boundary from being mistaken for a gap.

**Actors**: `cpt-cf-bss-products-actor-finance-reviewer`

### 6.13 Operational Resilience & Concurrency

#### Expected failure behavior

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-expected-failure-behavior`

Invalid/conflicting authoring **MUST** fail closed with an audited reason and **MUST NOT** partially apply, for each of: stale-revision write, duplicate idempotency key with different body, taxonomy cycle, unrecognized metering unit without elevation, publish of an incomplete entity, immutable-field change without a valid correction path, reissue of a reserved `skuCode` and concurrent `skuCode` collision, EOL retirement without an acknowledged migration consumer (post-v1), publishing a SKU under a non-`published` parent, a SKU scope falling outside its parent, authoring/cloning against a de-listed/deprecated unit, a bulk row whose in-batch dependency failed, adopting a `compositionPending` bundle, an indeterminate region overlap, and a retention process that would orphan a live grandfathered reference.

**Rationale**: A single enumerated fail-closed contract keeps negative paths deterministic and auditable.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Event delivery resilience

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-event-delivery-resilience`

The **shared event system** **MUST** provide bounded-backoff retry, per-consumer delivery state, and an **audited dead-letter** path with alerting (transport owned there). The **registry's own** obligations are limited to: not reporting emission success until the event is **durably accepted**, **surfacing** the per-consumer delivery/dead-letter state as a projection, and never mutating registry state on a delivery failure. During a bus outage, mutations **MAY** commit with events to a durable **outbox** for later emission; the propagation clock starts at durable bus acceptance, not at commit.

**Rationale**: Resilience mechanics belong to the bus; the registry must not falsely report propagation or lose events.

**Actors**: `cpt-cf-bss-products-actor-events-audit`

#### CatalogVersion publish concurrency

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-catalog-publish-concurrency`

Concurrent publishes within a tenant **MUST** serialize; `catalogVersionId` **MUST** be allocated monotonically without gaps or collisions; a staged entity mutated or retired between stage and publish **MUST** cause that publish to **re-validate fail-closed** (rejected, naming the changed entity) rather than freezing stale or partial content.

**Rationale**: Per-tenant serialization + re-validation guarantees no published version contains concurrently-invalidated content.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### Fail-safe duration tripwire

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-failsafe-tripwire`

While operating in `SkuReferenceCount`-unavailable fail-safe mode, when break-glass immutable-field corrections exceed a configured rate (interim > 5 in 30 days) the system **MUST** raise an escalation alert and **reclassify `SkuReferenceCount` delivery as a release blocker**, so unbounded degraded operation is detected and escalated, not normalized.

**Rationale**: The tripwire bounds the fail-safe operational debt in time, not merely acknowledging it.

**Actors**: `cpt-cf-bss-products-actor-catalog-admin`

#### `skuCode` reservation concurrency

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-skucode-reservation-concurrency`

For two concurrent reserve requests for the same `skuCode` within a tenant, the system **MUST** atomically reserve at **create** time, admit exactly one, and reject the other fail-closed with an audited reason; a `draft` reservation **MUST** block a second draft until released or discarded. A `skuCode` changed while still `draft` **MUST** release the previous code; **discarding a never-published draft MUST also release** its `skuCode`/`productCode` reservation (permanent reservation applies only from first publish).

**Rationale**: Atomic reservation prevents duplicate codes while letting abandoned drafts free the namespace.

**Actors**: `cpt-cf-bss-products-actor-product-manager`

#### Reference-producer registration

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-reference-producer-registration`

Only **registered** producers' signals or silence **MUST** factor into the `referenced` predicate; an unregistered producer's absence **MUST NOT** pin every SKU immutable. Producer-set membership **MUST** be a governed, audited change **snapshotted symmetrically with the freeze-participant set**, and onboarding a new producer **MUST NOT** retroactively flip historical mutability/retirement decisions.

**Rationale**: Registration prevents a not-yet-onboarded producer from freezing the whole catalog and keeps history stable.

**Actors**: `cpt-cf-bss-products-actor-subscriptions`

#### Grandfathered-snapshot retention coupling

- [ ] `p1` - **ID**: `cpt-cf-bss-products-fr-grandfathered-retention-coupling`

For a `catalogVersionId` referenced by ≥ 1 **live** grandfathered reference, the snapshot **MUST** remain byte-identically resolvable for as long as a live reference exists, **regardless of the statutory-max retention clock**; retention expiry **MUST** be gated on no live references to that `catalogVersionId`. Because per-SKU `SkuReferenceCount` carries no version dimension, version-liveness **MUST** be sourced from per-version freeze-registration records (or a `(catalogVersionId, producer)` producer contract), never from the SKU-level count alone. A retention process that would orphan a live reference **MUST** fail closed with an alert.

**Rationale**: Silently GC'ing a snapshot under a live contract breaks reproducibility for every reference frozen to it — a compliance event.

**Actors**: `cpt-cf-bss-products-actor-billing`

#### Pre-publish lint report

- [ ] `p2` - **ID**: `cpt-cf-bss-products-fr-prepublish-lint`

The `validate(lint)` operation before `CatalogVersion` publish **MUST** return a **structured, per-entity report** of every override-requiring or attention condition (uncomposed `bundle` SKUs, missing default-locale attribute values, declarations against a `deprecated` unit) so the two-person override is an **informed** decision and the audit records **what** was overridden.

**Rationale**: An informed override beats a blind acknowledgment and produces a meaningful audit trail.

**Actors**: `cpt-cf-bss-products-actor-finance-reviewer`

## 7. Non-Functional Requirements

### 7.1 NFR Inclusions

> Numeric targets are binding **design targets** until the program NFR workshop (scheduled within 2 weeks of PRD approval; named DRI = BSS Program Lead). Interim configurable-policy defaults are in §17.1.

#### Read latency

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-read-latency`

Browse/search reads within a tenant partition **MUST** meet **p95 < 100 ms** over a 5-minute window on a warm read model holding 10K SKUs/tenant with ≥ 100 concurrent readers, sustained via cache-first read models and tenant/brand/region partitioning.

**Threshold**: p95 < 100 ms @ 10K SKUs/tenant, ≥ 100 concurrent readers.

**Rationale**: Slow catalog reads degrade portal/sales UX.

#### Read throughput

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-read-throughput`

The cache-first read model **MUST** sustain **≥ 2,000 read QPS per tenant partition** at the read-latency target.

**Threshold**: ≥ 2,000 read QPS/tenant partition at p95 < 100 ms.

**Rationale**: Peak browse/search traffic must not breach latency.

#### Publication propagation

- [ ] `p2` - **ID**: `cpt-cf-bss-products-nfr-publication-propagation`

Downstream event availability (incl. fan-out) after an approved publish **MUST** occur within **< 3 s** — a component preceding freeze acks, distinct from read-model convergence (< 2 s) and end-to-end posting-safe (< 5 s).

**Threshold**: event availability < 3 s after publish (p99).

**Rationale**: Delayed publication yields stale offerings downstream; the three nested budgets must not collapse to one.

#### End-to-end posting-safe budget

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-posting-safe-budget`

From write commit to "posting-safe" (read-model converged **and** all participants' `freezeComplete` acknowledged) **MUST** be **p99 < 5 s**; if the freeze times out the version **MUST** remain non-posting-safe (fail closed). This composite is a **program-level SLO** decomposed into a registry-owned `commit → event-durably-published` budget and per-participant `event → ack` budgets.

**Threshold**: p99 < 5 s commit → posting-safe (fail-closed on freeze timeout).

**Rationale**: Downstream needs a single SLA to design against; freeze acks follow fan-out.

#### Snapshot archival & cold-resolution SLA

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-snapshot-archival-dr`

Cold `catalogVersionId` re-resolution **MUST** remain byte-identical and meet a looser-than-hot target (interim p95 < 2 s). `CatalogVersion` snapshots + version history are **financial records** with a durability class (interim **≥ 11 nines** / replicated storage), backup/restore with **periodic checksum restore verification**, and a cross-region/DR posture with RPO/RTO (set at the NFR workshop). Availability SLOs do **not** substitute for durability.

**Threshold**: cold p95 < 2 s; durability ≥ 11 nines; periodic restore verification; RPO/RTO TBD (workshop).

**Rationale**: Silently losing one snapshot breaks reproducibility for every contract frozen to it — a compliance event.

#### Scale & extensibility limits

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-scale-extensibility`

The system **MUST** support **≥ 10K SKUs per tenant** without breaching read latency, within configured limits (max attributes/entity, max taxonomy depth, max children/node). The scale model **MUST** also bound tenant count, total cardinality, and **`CatalogVersion` growth** (full-snapshot-per-publish is the dominant cost driver) with a publishes/day/tenant target set at the workshop.

**Threshold**: ≥ 10K SKUs/tenant; extensibility limits + publish-frequency target per workshop.

**Rationale**: Full-snapshot economics and extensibility limits bound the design.

#### Graceful degradation & staleness exposure

- [ ] `p2` - **ID**: `cpt-cf-bss-products-nfr-graceful-degradation`

Above the throughput ceiling or read-model lag, the system **MUST** shed or queue excess load **without ever serving cross-scope or unpublished content**, and **MUST** expose staleness via the **same `asOfCatalogVersion` mechanism** (one signal, machine-readable) — no silently-stale degraded response.

**Threshold**: zero cross-scope/unpublished leakage under overload; machine-readable `asOfCatalogVersion` on every stale response.

**Rationale**: Overload must never compromise isolation or hide staleness.

#### Determinism & integrity

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-determinism-integrity`

Version immutability, taxonomy acyclicity, SKU identity uniqueness, and metering-unit validity **MUST** be enforced fail-closed, and posted-period `CatalogVersion` snapshots **MUST** remain immutable.

**Threshold**: 100% fail-closed enforcement of the registry invariants.

**Rationale**: The registry is the integrity foundation all monetization binds to.

#### Backward-compatible schema evolution

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-backward-compatible-evolution`

A consumer pinned to schema `vN` **MUST** successfully deserialize a `vN+1` payload (new fields optional with defined defaults); a CI contract test **MUST** assert backward compatibility on every schema change.

**Threshold**: 100% `vN`→`vN+1` deserialization; CI-guarded on every schema change.

**Rationale**: New product categories/fields must not break published content or downstream contracts.

#### Availability & audit completeness

- [ ] `p1` - **ID**: `cpt-cf-bss-products-nfr-availability-audit`

The cache-first **read** path **MUST** meet **99.9%** availability and the **write/publish** path **99.5%** (reads must not block downstream when writes degrade); write paths **MUST** be fully audited even during partial failures.

**Threshold**: read 99.9% / write 99.5% availability; 100% write-path audit.

**Rationale**: Reads feeding portals/sales must stay up independently of write degradation.

### 7.2 NFR Exclusions

- **Pricing/rating performance** — owned by plan-price / Tariffs / Rating; the registry only serves published primitives.
- **Usage collection/normalization throughput** — OSS metering / Usage Collector.
- **Event-bus transport SLOs (delivery latency, DLQ retention)** — owned by the common event system (Common Core); the registry states only its own emission/projection obligations.
- **Storefront UX performance / accessibility (WCAG) / i18n rendering** — Presentation layer / frontend DESIGN.
- **Marketplace listing/search performance** — Marketplace PRD (§4.8).

## 8. Five Quality Vectors Analysis

| **Quality Vector** | **Show-Stopper Requirements** | **Rationale** |
|--------------------|-------------------------------|---------------|
| **Efficiency** | Product/SKU/category/attribute changes MUST be operator self-service with automated versioning and approvals — no engineering involvement for routine registry change. | Manual catalog management blocks catalog growth and time-to-market. |
| **Reliability** | Immutable Product/SKU versioning, byte-identical `CatalogVersion` snapshots, 100% audited write paths, fail-closed publish validation, and a CI contract test guarding the registry↔plan-price seam. | The registry is the foundation all monetization binds to; silent drift or lost history breaks downstream snapshots and compliance. |
| **Performance** | Browse/search p95 < 100 ms and ≥ 2,000 read QPS/tenant partition via cache-first read models; nested propagation budgets (convergence < 2 s, propagation < 3 s, posting-safe < 5 s, cold-version < 2 s); graceful degradation. | Slow catalog reads degrade portal/sales UX; delayed publication yields stale offerings. |
| **Security** | Complete tenant/brand/region isolation (deny-by-default), RBAC, two-person rule for material changes, time-boxed break-glass for cross-tenant access, retention/erasure reconciled with immutable audit, minimal PII in events. | Cross-tenant leakage is a commercial risk; unauthorized publish is a fraud risk; standing super-access and unbounded retention are compliance risks. |
| **Versatility** | Extensible attributes/taxonomy and a type-agnostic SKU model (product/service/bundle) with backward-compatible schema evolution (`vN` deserializes `vN+1`). | New product categories must be added without breaking published content or downstream contracts. |

## 9. Public Library Interfaces

> The registry is a backend service, not a client library. Interfaces below are high-level contracts; concrete API schemas, endpoints, event payloads, and DDL belong in DESIGN.

### 9.1 Public API Surface

#### Catalog authoring & publish

- [ ] `p1` - **ID**: `cpt-cf-bss-products-interface-authoring-publish`

**Type**: command/authoring + approval-gated publish API (shape in Design)

**Stability**: stable (contract intent), schema unstable (Design owns)

**Description**: Create/update Products, SKUs, categories, attributes, `PlanTier`; declare metering units and accounting codes; lifecycle transitions; two-person-gated publish of entities and `CatalogVersion`; idempotent by key. Requires the resolution caller to **declare intent** (`browse` vs `posted/contractual`).

**Breaking Change Policy**: Major version bump; idempotency-key and intent-declaration semantics are part of the contract.

#### Catalog read model (browse/search)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-interface-read-model`

**Type**: cache-first query/read API (shape in Design)

**Stability**: stable (contract intent)

**Description**: Tenant/brand/region-scoped browse/search/filter of published Products/SKUs/Categories with the per-state visibility contract and an `asOfCatalogVersion` staleness signal; version-history retrieval.

**Breaking Change Policy**: Major version bump for incompatible query/response changes.

### 9.2 External Integration Contracts

#### CatalogVersionPublished + registry events (outbound)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-contract-registry-events`

**Direction**: provided by the registry to the shared event system

**Protocol/Format**: CloudEvents 1.0 with `dataschema`+semver, correlation/causation, per-aggregate ordering keys, pseudonymous actor refs; includes `CatalogVersionPublished` and the full Product/SKU/Category/Attribute/governance event set (Design owns names/schemas).

**Compatibility**: `vN` consumer deserializes `vN+1`; bootstrap path (latest `CatalogVersion` + tail); no direct PII.

#### `SkuReferenceCount` signal (inbound)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-contract-sku-reference-count`

**Direction**: required from Subscriptions, Contracts, plan-price

**Protocol/Format**: per-producer watermark ("as of `T`, complete live-reference set is {…}"); freshness on the watermark; registered producers only (Design owns shape).

**Compatibility**: absence under a fresh watermark ⇒ zero; boolean OR across producers; stale/never-received ⇒ conservatively referenced + alert. **Pre-approval gate**: owner + delivery date (§15).

#### Freeze acknowledgment (inbound)

- [ ] `p1` - **ID**: `cpt-cf-bss-products-contract-freeze-ack`

**Direction**: required from plan-price, Contracts, Billing

**Protocol/Format**: per-`catalogVersionId` freeze acknowledgment feeding `freezeComplete` (Design owns shape).

**Compatibility**: bounded timeout fails closed; participant set snapshotted per `catalogVersionId`; force-completion records missing participants as not-frozen.

#### Bundle composition-completed signal (inbound)

- [ ] `p2` - **ID**: `cpt-cf-bss-products-contract-bundle-composition-signal`

**Direction**: required from plan-price

**Protocol/Format**: signal that a `bundle` SKU has been composed, clearing `compositionPending` (Design owns shape).

**Compatibility**: clearing produces a new published version and emits `BundleCompositionCompleted`; MUST NOT mutate a prior frozen `CatalogVersion`.

## 10. Use Cases

#### Author products and SKUs

- [ ] `p1` - **ID**: `cpt-cf-bss-products-usecase-product-sku-editor`

**Actor**: `cpt-cf-bss-products-actor-product-manager`

**Preconditions**:
- Established tenant/brand/region context; CatalogAdmin or ProductManager role.

**Main Flow**:
1. Create/select a Product (name, category, description, brand/region scope); `productId`/`skuId` system-generated, `skuCode` operator-entered with inline format check.
2. Add a SKU; pick type product/service/bundle.
3. For a usage SKU, declare the metering unit (validated); set `PlanTier` from the taxonomy.
4. Set tax/GL codes; save as draft (each save bumps the internal revision).

**Postconditions**:
- A draft Product/SKU exists with reserved codes and an audit entry, pending gated publish.

**Alternative Flows**:
- **Incomplete at publish**: publish rejected fail-closed (missing required fields / `PlanTier` / accounting codes).

#### Approve and publish registry changes

- [ ] `p1` - **ID**: `cpt-cf-bss-products-usecase-approval-publish`

**Actor**: `cpt-cf-bss-products-actor-finance-reviewer`

**Preconditions**:
- Pending material change(s) in the approval queue.

**Main Flow**:
1. Open the pending-approval queue; review the diff and (for `CatalogVersion` publish) the pre-publish lint report.
2. Approve/reject with reason; two-person rule enforced above threshold (≥ 1 FinanceReviewer for finance-material fields).
3. On full sign-off, publish triggers the entity/`CatalogVersion` publish + events.

**Postconditions**:
- Content published (or returned to draft with reason); approval pinned to the approved revision.

**Alternative Flows**:
- **Edit after approval**: approval invalidated, change re-queued with diff re-presented.

#### Deprecate and retire safely

- [ ] `p1` - **ID**: `cpt-cf-bss-products-usecase-lifecycle-deprecation`

**Actor**: `cpt-cf-bss-products-actor-catalog-admin`

**Preconditions**:
- A `published` SKU with (possibly) active references.

**Main Flow**:
1. Mark `deprecated` (blocks new adoption; existing continue) or un-deprecate (two-person).
2. Initiate retire/EOL; the system shows the active-reference count and requires confirmation.
3. Confirm; snapshots preserved; retirement event emitted with lead-time and optional `replacedBy`.

**Postconditions**:
- Scheduled transition set; existing references grandfathered on frozen snapshots.

**Alternative Flows**:
- **Cascade with EOL-requiring children**: those children listed and left un-retired; parent stays non-`retired` with deferred-retire intent tracked.

#### Browse, search, and inspect history

- [ ] `p2` - **ID**: `cpt-cf-bss-products-usecase-catalog-browser-history`

**Actor**: `cpt-cf-bss-products-actor-auditor`

**Preconditions**:
- Published catalog content and version history exist for the scope.

**Main Flow**:
1. Filter/search (category, status, brand/region) on the cache-first read model.
2. Open an item; view attributes/classification.
3. Open the version timeline with diffs and audit entries (actor, time, correlation ID).

**Postconditions**:
- Offerings found; change lineage traced (tenant-scoped).

**Alternative Flows**:
- **Cross-tenant inspection**: requires time-boxed break-glass (read/audit-export only), individually audited.

#### Bulk import/export at scale

- [ ] `p1` - **ID**: `cpt-cf-bss-products-usecase-bulk-operations`

**Actor**: `cpt-cf-bss-products-actor-catalog-admin`

**Preconditions**:
- A CSV/JSON batch of Products/SKUs/Categories.

**Main Flow**:
1. Upload/import; rows land as **draft** with per-row validation and per-row idempotency.
2. Review the aggregated change report (counts, per-type summary, sample, lint findings).
3. Submit the batch for gated approval (two-person on the batch); track per-row success/failure.

**Postconditions**:
- Rows applied consistently (no orphan/partial publish); a coalesced `CatalogBulkOperationCompleted` emitted.

**Alternative Flows**:
- **Dependent-row failure**: dependent rows fail with a distinct per-row error; no orphan committed.

#### Inspect and recover a stuck freeze

- [ ] `p2` - **ID**: `cpt-cf-bss-products-usecase-freeze-monitoring`

**Actor**: `cpt-cf-bss-products-actor-catalog-admin`

**Preconditions**:
- A `CatalogVersion` whose `freezeComplete` has not been reached past the timeout.

**Main Flow**:
1. View per-`catalogVersionId` `freezeComplete` status and each non-acknowledging participant.
2. Idempotently re-trigger the freeze fan-out.
3. Force-complete under the two-person rule (records missing participants as not-frozen; emits `FreezeForceCompleted`).

**Postconditions**:
- Freeze resolved or force-completed with missing participants pinned fail-closed for posted use.

## 11. User Interaction and Design

| **Interface Name** | **Role** | **Steps** | **Mockup Screen** |
|--------------------|----------|-----------|-------------------|
| Product & SKU editor | As a Product Manager, I define products and SKUs so the catalog foundation is accurate | 1. Create/select Product (name, category, description, scope); `productId`/`skuId` system-generated, `skuCode` operator-entered with format check<br>2. Add SKU; pick type<br>3. For usage SKU declare metering unit; set PlanTier<br>4. Set tax/GL codes<br>5. Save as draft | — |
| Category & taxonomy manager | As a Product Manager, I organize products into a taxonomy so browse/search and listings are coherent | 1. Open taxonomy tree<br>2. Create/re-parent Category (uniqueness + cycle checks)<br>3. Assign products to categories | — |
| Attribute & localization editor | As a Product Manager, I add localized attributes so products read correctly per brand/region | 1. Open attribute editor<br>2. Add key/value; enable i18n; add locale values<br>3. Set brand/region visibility; configure fallback | — |
| Catalog approval & publish console | As a Finance Reviewer, I review and approve registry changes before publication | 1. Open pending-approval queue<br>2. Review diff (+ lint report)<br>3. Approve/reject with reason; two-person above threshold<br>4. On sign-off, publish triggers CatalogVersion + events | — |
| Lifecycle & deprecation manager | As a Product Manager, I deprecate and retire SKUs safely so existing references are unaffected | 1. Select published SKU<br>2. Deprecate / un-deprecate (two-person)<br>3. Initiate retire/EOL; system shows active-reference count and optional `mustMigrateBy`<br>4. Confirm; snapshots preserved; retirement event with lead-time | — |
| Catalog browser & version history | As a Partner/Auditor, I browse/search and inspect version history to find offerings and trace changes | 1. Filter/search (category, status, brand/region) on cache-first read model<br>2. Open item; view attributes/classification<br>3. Open version timeline with diffs and audit entries | — |
| Bulk operations console | As a CatalogAdmin, I import/export catalog entities in bulk so onboarding and mass edits scale | 1. Upload/import (CSV/JSON); rows land as draft with per-row validation<br>2. Review aggregated change report (counts, summary, sample, lint)<br>3. Submit for gated approval; track per-row status<br>4. Export a deterministic snapshot for a `catalogVersionId` | — |
| Freeze monitoring & recovery console | As an Operator, I inspect and recover stuck cross-module freezes so posting is never blocked silently | 1. View per-`catalogVersionId` `freezeComplete` and non-acknowledging participants<br>2. Idempotently re-trigger the fan-out<br>3. Force-complete under two-person (emits `FreezeForceCompleted`) | — |
| Operational console (clone, break-glass, deferred retire) | As an Operator/Platform owner, I want clone, break-glass, and deferred-cascade actions in one place | 1. Clone a Product/SKU (incl. a retired source) to a new draft<br>2. Break-glass cross-tenant read/audit-export under time-boxed elevation<br>3. Resume a deferred cascade-retire once blocked children clear | — |

## 12. Acceptance Criteria

**As a** Product Manager, Catalog Admin, or Finance Reviewer, **I want** an authoritative, governed, versioned catalog registry **so that** plan/price authoring, subscriptions, contracts, rating, and billing build on stable, reproducible product definitions.

### Identifiers & Integrity

**1. Identifier contract**
- **Given** a Product or SKU being created
- **When** the system assigns identifiers
- **Then** `productId`/`skuId` MUST be server-generated immutable identifiers (never operator-supplied); `skuCode` MUST be short, fixed-format, tenant-unique, reserved **atomically at create**, and immutable after first publish
- **And** downstream consumers MUST bind to `skuId`; a reused/malformed `skuCode` MUST be rejected with an audited reason; once first published a `skuCode` is permanently reserved within the tenant
- **And** a Product MAY carry an optional `productCode` under the same rules; when unset, product-level external mapping is `productId`-only

**2. Product/SKU field-mutability matrix**
- **Given** a published Product or SKU
- **When** an operator edits it
- **Then** mutability MUST be classified by lifecycle state: structural identity immutable (remedied only by retire + clone); `type`/metering-unit immutable-but-correctable via the fresh-zero path; material-but-mutable (`PlanTier`/`taxCategory`/`glCode`/`sellable`) via a new published version under governance; other fields via a new version
- **And** an illegal change MUST be rejected fail-closed with an audited reason
- **And** the active-reference count MUST be sourced from `SkuReferenceCount` as the 3-state predicate; never treat an entity as unreferenced absent a fresh watermark

**2a. Sellable flag (offering eligibility)**
- **Given** a SKU with `sellable = false` (composition/metering-only; default is `true`)
- **When** it is published and referenced
- **Then** publish MUST succeed and bundle/plan **component** references MUST remain valid, while any **standalone** offer of the SKU MUST fail the plan-price sellability gate (predicate 6)
- **And** flipping `sellable` MUST follow the material-but-mutable path (new published version, governed) and the value MUST be frozen per `CatalogVersion`

**3. Reference-signal sourcing, freshness & counting**
- **Given** the `SkuReferenceCount` signal (per-producer watermark; owner/delivery date is a pre-approval gate)
- **When** the registry evaluates mutability/correction/retirement
- **Then** absence of a `skuId` under a fresh watermark ⇒ zero for that producer; `referenced` MUST be a boolean OR across registered producers; the registry MUST NOT sum across producers
- **And** a fresh-zero across all producers ⇒ unreferenced; stale ⇒ conservatively referenced + alert; never-received ⇒ conservative + distinct flag
- **And** only registered producers count; Contracts MUST declare whether draft/quote refs count, identically across AC #2/#4/#18; until the signal ships, #2/#4/#18 run fail-safe

**4. Immutable-field correction (zero-reference & break-glass)**
- **Given** a published SKU whose correctable immutable field (`type` or metering-unit) was set wrong
- **When** a CatalogAdmin requests a correction
- **Then** if `SkuReferenceCount` is fresh-zero across all producers the system MUST allow a governed re-publish (two-person), bump the version, and emit `SkuImmutableFieldCorrected`; absent a fresh-zero signal it MUST reject fail-closed
- **And** while the signal is entirely unavailable, correction MAY proceed only via break-glass (two-person + reason + `SkuCorrectionOverride` recording signal-unavailability), feature-flag OFF by default

### Product & Taxonomy Definition

**5. Create a product**
- **Given** a CatalogAdmin/ProductManager in a tenant/brand/region context
- **When** they create a Product (primary category optional at draft, required at publish)
- **Then** the system MUST generate `productId`, persist as `draft` (version 0) with isolation + audit
- **And** name uniqueness MUST hold on `(tenantId, brandId, normalized(name))` within any overlapping region scope; two same-named Products allowed only when region scopes are disjoint
- **And** indeterminate region overlap MUST fail closed with an operator-facing reason; the region-set algebra is a pre-approval gate

**6. Manage taxonomy (create, rename, re-parent, retire, delete)**
- **Given** a CatalogAdmin managing the taxonomy
- **When** they create/rename/re-parent/retire/delete a Category
- **Then** the system MUST validate uniqueness within parent (re-checked on rename/re-parent), reject cycles, and reject exceeding max depth/children
- **And** a Product carries exactly one primary + zero-or-more secondary categories; the read model MUST make it filterable under every assigned category
- **And** categories are governed live entities (in-place, two-person-gated, audited); retire/delete MUST be blocked while any active Product references it or active children exist; each op emits a `Category*` event

### SKU Definition & Classification

**7. Define a SKU**
- **Given** an existing Product
- **When** a ProductManager defines a SKU typed `product`/`service`/`bundle`
- **Then** the system MUST link it, assign `skuId`/`skuCode`, and validate the per-type required-field set
- **And** a `bundle` SKU persists only type flag + identity (composition in plan-price; blocking completeness at `CatalogVersion` publish)
- **And** promotional/$0/"Free" SKUs follow identical registry rules; no separate promo entity

**8. Declare metering unit (defines a "usage SKU")**
- **Given** a SKU to be metered (declaring a unit is what defines a usage SKU)
- **When** a ProductManager declares its metering unit
- **Then** the system MUST validate against the configured recognized-unit set and reject an unrecognized unit unless elevated approval marks a new validated unit
- **And** a usage SKU declares exactly one unit (single-dimension); multi-dimension via separate SKUs composed at plan/bundle level
- **And** a draft whose unit was `deprecated` before first publish MUST be treated as a new declaration and rejected; the declared unit MUST be carried on publish

**9. Metering-unit de-listing**
- **Given** a recognized unit referenced by ≥ 1 published SKU
- **When** an operator attempts to de-list it
- **Then** the system MUST reject removal while live references exist and instead support marking it `deprecated` (no new declarations), with full removal only once unreferenced
- **And** a unit's identity/semantics are immutable (no silent GB→GiB); a correction is a new unit + deprecation; de-listing MUST be audited and MUST NOT mutate any frozen snapshot

**10. PlanTier classification**
- **Given** a SKU being authored
- **When** a ProductManager assigns its `PlanTier`
- **Then** the system MUST validate against the taxonomy and carry it on the published SKU
- **And** a `PlanTier` value has a stable tier code; rename affects the display label only; taxonomy management is governed (two-person), emits `PlanTierUpdated`, and a value MUST NOT be retired while any published SKU carries it; seeded with a neutral value
- **And** `PlanTier` MUST NOT be conflated with OrgTier; plan-publish presence enforcement is delegated to plan-price

**11. Stable accounting codes on SKU**
- **Given** a SKU
- **When** a ProductManager sets tax-category and GL codes
- **Then** the system MUST persist them as stable codes and validate each against a configured recognized set (owner Finance)
- **And** the codes are required at publish for `product`/`service`-type SKUs; a SKU published without a required code MUST be rejected
- **And** the system MUST NOT compute tax or post to GL (codes only)

### Attributes & Localization

**12. Localized attributes, well-known display fields, and definition lifecycle**
- **Given** a Product/SKU with attributes
- **When** a ProductManager adds i18n values with brand/region visibility
- **Then** the system MUST validate against the attribute definition, require the default-locale value, and resolve locale via `(locale, region, brand) → (locale, brand) → (default-locale, brand) → global`
- **And** the registry MUST seed well-known display attribute definitions (localized display name/description for Product/SKU/Category)
- **And** managing definitions is a governed live-entity op emitting `AttributeDefinitionUpdated`; changes MUST be backward-compatible with a deprecate-then-remove lifecycle
- **And** the registry MUST provide an ungoverned, size-bounded, non-localized, search-excluded, PII-prohibited metadata map for machine metadata

### Versioning, Lifecycle & Deprecation

**13. Revision vs published version**
- **Given** a Product/SKU
- **When** an operator saves a draft edit
- **Then** the system MUST bump the internal revision (every save) and reject stale-revision writes, while the published version bumps only on publish
- **And** consumers and `CatalogVersion` MUST reference the published version; historical versions retained with a diff, non-modifiable
- **And** version binding MUST be explicit; a bound-but-not-yet-frozen reference re-resolving to a different version at freeze MUST surface a version-change diff, not silently swap

**14. Lifecycle transitions & reversibility**
- **Given** the state machine `draft → published [↔ deprecated] → retired` (+ `draft → discarded`)
- **When** an operator requests a transition
- **Then** the system MUST allow only defined transitions, treat `retired`/`discarded` as terminal, and allow referencing only for `published`/`deprecated`
- **And** entity publish MAY be scheduled (`publishAt`, UTC) with approval pinned at scheduling and re-validated fail-closed at activation
- **And** there is no `unpublish` and no in-place rollback — retraction/reversion is `deprecate`/`retire` + a new version (forward-only)

**15. Parent-child (Product↔SKU) lifecycle integrity**
- **Given** the Product↔SKU hierarchy
- **When** an operator publishes or retires across the hierarchy
- **Then** a SKU MUST NOT reach `published` under a non-`published` parent, and MUST NOT be orphaned under a `retired` Product; a SKU's scope MUST be contained within its parent's
- **And** retiring a Product with non-`retired` SKUs MUST require confirmed cascade-retire (partial by design, recording `direct` vs `cascaded`); EOL-requiring children are listed and left un-retired; never-published children auto-`discarded`
- **And** when a partial cascade leaves children, the parent MUST remain non-`retired` with deferred-retire intent tracked/queryable

**16. Deprecation (governed sub-state)**
- **Given** a published SKU referenced by active plans/subscriptions/contracts
- **When** an operator marks it `deprecated`
- **Then** the system MUST move it to the `deprecated` sub-state, mark it so consumers block new adoption while existing references continue, and emit `SkuDeprecated`
- **And** `deprecated` MUST be a tracked, queryable state recording provenance `direct` (vs `cascaded`)

**17. Un-deprecation**
- **Given** a `deprecated` SKU
- **When** an authorized operator un-deprecates it
- **Then** `deprecated → published` MUST be allowed under the two-person rule, re-open new adoption, and emit `SkuUndeprecated`
- **And** un-deprecating a Product reverses only `cascaded` child deprecations, never a `direct` one; a `retired` entity MUST NOT be reversible

**18. Retirement / EOL consumer handoff**
- **Given** a `published`/`deprecated` SKU with active references
- **When** an operator retires it (optionally EOL with `mustMigrateBy`)
- **Then** the system MUST require confirmation with the active-reference count shown, then run a scheduled transition: force `deprecated` at initiation, preserve snapshots, emit `SkuRetired`/`ProductRetired` with `{ skuId, fromVersion, reason, replacedBy?, mustMigrateBy?, effectiveAt }` honoring the ≥ 30-day lead-time, then flip to `retired` at `effectiveAt`
- **And** the registry is SoR for `replacedBy` (a successor published SKU)
- **And** v1 = plain retirement + grandfathering only; EOL-with-`mustMigrateBy` is post-v1, disabled until the subscriptions-lifecycle AC exists and is referenced by number, and requires a consumer ack contract (lapsed ack ⇒ suspend fail-closed + `SkuEolSuspended`)

### Catalog Versioning & Snapshots

**19. Publish an immutable catalog version**
- **Given** approved catalog changes
- **When** a CatalogAdmin publishes a `CatalogVersion`
- **Then** the system MUST persist a full snapshot (published Product/SKU set + versions + current categories/attributes), assign a monotonic `catalogVersionId`, generate a checksum, record timestamps, capture the freeze-participant set, and make it immutable
- **And** an uncomposed `bundle` SKU MUST require explicit two-person override (blocking-with-override), recorded in audit, and be flagged `compositionPending = true`
- **And** it MUST emit `CatalogVersionPublished` and expose per-version `freezeComplete`; a published version cannot be withdrawn/rolled back (roll-forward N+1 only); publishes serialize per tenant

**20. Snapshot reproducibility**
- **Given** a posted invoice or active contract that referenced a `catalogVersionId`
- **When** the catalog later changes
- **Then** re-resolving that `catalogVersionId` MUST yield a byte-identical checksum and unchanged registry content
- **And** `CatalogVersion` MUST be exposable as one component of a downstream `pricingSnapshotRef` without asserting it equals the full snapshot

**21. Cross-module snapshot freeze atomicity**
- **Given** a `CatalogVersionPublished` consumed by the registered freeze-participants
- **When** a consumer resolves `catalogVersionId` before all have frozen
- **Then** the system MUST expose `freezeComplete` and reject resolution for posted/contractual use until all participants ack, with a bounded timeout that fails closed
- **And** read-only browse MAY proceed during the freeze window
- **And** the resolution API MUST require the consumer to declare intent (`browse` vs `posted/contractual`) so it cannot post against a not-yet-`freezeComplete` version by mislabeling

**22. Freeze recovery & force-completion**
- **Given** a `CatalogVersion` past the freeze timeout
- **When** an operator inspects it
- **Then** the system MUST identify each non-acknowledging participant, support an idempotent re-trigger, and support force-completion (two-person) that records each missing participant as not-frozen and emits `FreezeForceCompleted`
- **And** force-completion MUST NOT mark missing content as frozen; the default is pinned fail-closed for that participant's content

**23. Freeze-participant set governance**
- **Given** the set of freeze-participants
- **When** the set changes
- **Then** membership MUST be a governed (two-person), audited change, and each `CatalogVersion` MUST snapshot the participant set at publish time
- **And** a participant removed after publish MUST NOT retroactively flip that version's `freezeComplete`

**24. Grandfathering invariant**
- **Given** a reference grandfathered onto a frozen snapshot after its SKU is deprecated/retired
- **When** the catalog subsequently changes
- **Then** the registry MUST guarantee the grandfathered snapshot is never mutated
- **And** grandfathering eligibility policy is owned by plan-price / subscriptions-lifecycle; this AC makes the delegation auditable

**25. Uncomposed-bundle adoption guard**
- **Given** a `bundle` SKU published with the uncomposed override
- **When** the read model and events expose it
- **Then** it MUST carry `compositionPending = true` until composed, and consumers MUST treat it as not-yet-adoptable for new references
- **And** clearing it MUST be driven by a plan-price composition signal, audited, and emitted as `BundleCompositionCompleted` (new version, never mutating a prior frozen `CatalogVersion`)

### Approval, Publishing & Eventing

**26. Materiality-gated publish**
- **Given** a Product/SKU change or a material Category/attribute-definition op
- **When** the change is material (touches `PlanTier`/metering-unit/`taxCategory`/`glCode`, a lifecycle transition, a Category create/rename/re-parent/retire/delete, a material attribute-definition change, or exceeds the configured affected-entity count)
- **Then** the system MUST enforce ≥ two distinct approvers, each distinct from the author and holding CatalogAdmin or FinanceReviewer; a finance-material field MUST include ≥ 1 FinanceReviewer
- **And** an approval MUST be pinned to the internal revision; any subsequent edit invalidates it and re-queues with the diff re-presented
- **And** the rule MUST be a typed configurable policy with an enforceable interim default (§17.1); a rejection returns the entity to `draft` with reason; v1 uses a single two-person step

**27. Idempotent authoring boundary**
- **Given** a retried create/update/publish carrying an idempotency key
- **When** the system processes the retry
- **Then** the same key + identical payload MUST NOT create duplicate entities/versions/events; the key MUST be scoped per tenant + endpoint + client key and retained ≥ 24h and ≥ the max freeze timeout
- **And** reuse with a different payload MUST be rejected as a conflict (no silent no-op)

**28. Registry eventing & audit**
- **Given** any state-changing registry mutation that completes
- **When** the write commits
- **Then** the registry MUST publish the corresponding CloudEvents 1.0 event onto the shared event system with correlation/causation + idempotency key + ordering keys `(tenant, aggregate)`; every state-changing AC maps to exactly one named event (or an explicit "no event" in Design)
- **And** payloads MUST carry pseudonymous actor references only (never direct operator PII); the mutation MUST be recorded in an immutable, queryable audit trail
- **And** Plan/Price/Bundle-composition events MUST NOT be emitted here (owned by plan-price)

**29. Event schema versioning & replay**
- **Given** the CloudEvents of AC #28
- **When** the schema evolves or a consumer must rebuild state
- **Then** every event MUST carry a `dataschema` URI with a semantic version; a consumer pinned to `vN` MUST deserialize `vN+1`; out-of-order/duplicate delivery beyond the idempotency window MUST be detectable via `(tenant, aggregate, sequence)`
- **And** the system MUST provide a bootstrap path (latest `CatalogVersion` + event tail) for published-scope consumers and MUST fail loudly when a consumer checkpoint predates the available event tail

### Multi-Tenancy & Read Models

**30. Tenant/brand/region isolation & break-glass**
- **Given** a user scoped to one tenant/brand/region
- **When** they query/mutate outside their scope
- **Then** the system MUST deny by default at the gateway and audit the cross-scope attempt
- **And** privileged cross-tenant access MUST use time-boxed, reason-required, alertable break-glass, itself two-person-approved or post-hoc-reviewed; standing cross-tenant access MUST NOT be granted

**31. Break-glass action scope**
- **Given** a platform owner under break-glass elevation
- **When** they access a foreign tenant's catalog
- **Then** break-glass MUST permit read and audit-export only; any write/publish MUST be separately gated (two-person + distinct alert) or disallowed in v1
- **And** every break-glass action MUST be individually audited with the reason and correlation ID

**32. Cache-first browse/search with bounded convergence**
- **Given** published Products/SKUs/Categories
- **When** a partner/customer browses/searches/filters
- **Then** the system MUST serve from cache-first read models scoped to the caller's tenant/brand/region, converging within its own budget (interim p99 < 2 s)
- **And** stale reads during the window MUST be safe (never expose unpublished/cross-scope content) and carry the `asOfCatalogVersion` staleness signal
- **And** the per-state visibility contract MUST hold: `published` browsable; `deprecated` browsable + flagged + excludable; `retired` excluded from default browse, retrievable via explicit history query

### Bulk Operations

**33. Bulk import/export**
- **Given** a CatalogAdmin importing/exporting in bulk
- **When** the batch is processed
- **Then** the system MUST apply per-row idempotency, report per-row success/failure (no hidden partial failure), and never leave a partially-inconsistent published state
- **And** dependent rows MUST apply two-phase or dependency-ordered, never committing an orphan; idempotency operates at batch + per-row levels; a coalesced `CatalogBulkOperationCompleted` is emitted (no event storm)
- **And** bulk import lands entities in `draft`; publication remains gated, approved against an aggregated change report; export MUST be deterministic for a given `catalogVersionId`

### Cloning

**34. Clone a product/SKU**
- **Given** a source Product/SKU (draft, published, or retired — the sanctioned revival path)
- **When** a ProductManager clones it
- **Then** the system MUST create a new `draft` with new `productId`/`skuId` and a new `skuCode`/optional `productCode`, copying structure/attributes/scoping/category/`PlanTier`/metering-unit, resetting lifecycle and version counters, and never copying pricing/plan content
- **And** the cloned metering unit, `PlanTier`, and category assignment MUST be re-validated against live registries; the clone MUST fail or force re-selection if any was de-listed/deprecated/retired; a `clonedFrom` reference is recorded and the source unaffected

### Data Retention & Erasure

**35. Retention & right-to-erasure**
- **Given** retired entities, historical versions, and audit records
- **When** a retention or erasure (GDPR/CCPA) request applies
- **Then** the system MUST retain financial/version/audit records for the configured duration and satisfy erasure of actor PII by pseudonymizing it across audit, entity version fields, and the actor identity-reference map — not deleting immutable records
- **And** attribute/description free-text MUST NOT contain personal data — enforced by a validation block at write (hard prohibition, no carve-out, fail-closed on uncertainty, curated allow-list); Legal sign-off recorded in the approval artifact
- **And** erasure MUST NOT break `CatalogVersion` reproducibility or audit completeness

### Cross-PRD Consistency

**36. Registry ↔ plan-price seam**
- **Given** the shared contract on `skuId`, `bundle` type, metering-unit declaration, `PlanTier`, and `CatalogVersion`
- **When** registry or plan-price schemas change
- **Then** there MUST be a shared schema-version pin and a CI contract test that fails on divergence; a runtime divergence MUST fail closed (reject the dependent plan publish)
- **And** the same suite MUST assert consumer-side obligations: reject adoption of `compositionPending`/`deprecated` SKUs; reject a usage binding with no declared unit (and reject/warn on a `deprecated` unit) — where usage-completeness is enforced; consume `mustMigrateBy` (post-v1); resolve grandfathered refs against the frozen snapshot; re-validate on `SkuImmutableFieldCorrected`; declare intent before `freezeComplete`
- **And** each consumer-side assertion is authorable only once the referenced counterpart AC exists; (d) `mustMigrateBy` is deferred with the post-v1 EOL capability

**37. Monetization-model traceability**
- **Given** the deliberate decision that the registry carries no monetization-model marker (only `usage` leaves a footprint)
- **When** a reader asks which models are supported and where
- **Then** the PRD MUST expose a traceability map (§17.2): flat/per-seat/tiered/volume/hybrid/commitment → plan-price + Tariffs; usage → metering-unit declaration here + binding/rating downstream
- **And** absence of a model marker on a SKU MUST be treated as intentional, not a missing field

### Error & Negative Paths

**38. Expected failure behavior**
- **Given** an invalid or conflicting authoring request
- **When** the system processes it
- **Then** it MUST fail closed with an audited reason and MUST NOT partially apply, for each of the enumerated cases (see the `expected-failure-behavior` FR): stale-revision write, duplicate idempotency key with a different body, taxonomy cycle, unrecognized unit without elevation, publish of an incomplete entity, immutable-field change without a valid correction path, reissue/collision of a reserved `skuCode`, EOL without an acknowledged migration consumer (post-v1), SKU under a non-`published` parent, SKU scope outside its parent, authoring/cloning against a de-listed/deprecated unit, a bulk row whose in-batch dependency failed, adopting a `compositionPending` bundle, an indeterminate region overlap, and a retention process that would orphan a live grandfathered reference

### Operational Resilience & Concurrency

**39. Event delivery resilience**
- **Given** a registry event fanned out to ≥ 1 consumer
- **When** a delivery fails, a consumer never acks, or an event is poison
- **Then** the shared event system MUST provide bounded-backoff retry, per-consumer delivery state, and an audited dead-letter path with alerting (transport owned there)
- **And** the registry's own obligations are: not reporting emission success until durably accepted, surfacing per-consumer delivery/DLQ state as a projection, and never mutating registry state on delivery failure; during a bus outage mutations MAY commit with events to a durable outbox, with the propagation clock starting at durable bus acceptance

**40. CatalogVersion publish concurrency**
- **Given** two staged sets of changes targeting publication within one tenant
- **When** publishes are submitted concurrently, or a publish races a `deprecate`/`retire` on an entity it enumerates
- **Then** publishes MUST serialize per tenant, `catalogVersionId` MUST be allocated monotonically without gaps/collisions, and a staged entity mutated/retired between stage and publish MUST cause that publish to re-validate fail-closed (rejected, naming the changed entity)

**41. Fail-safe duration tripwire**
- **Given** the registry operating in `SkuReferenceCount`-unavailable fail-safe mode
- **When** break-glass immutable-field corrections exceed the configured rate (interim > 5 in 30 days)
- **Then** the system MUST raise an escalation alert and reclassify `SkuReferenceCount` delivery as a release blocker, so degraded operation is escalated, not normalized

**42. `skuCode` reservation concurrency**
- **Given** two concurrent create/reserve requests for the same `skuCode` within one tenant
- **When** both are processed
- **Then** the system MUST atomically reserve at create, admit exactly one, and reject the other fail-closed with an audited reason; a `draft` reservation MUST block a second draft until released/discarded
- **And** a `skuCode` changed while still `draft` MUST release the previous code; discarding a never-published draft MUST also release its `skuCode`/`productCode` reservation

**43. Reference-producer registration**
- **Given** the set of `SkuReferenceCount` producers (Subscriptions, Contracts, plan-price)
- **When** the registry evaluates `referenced`
- **Then** only registered producers' signals or silence MUST factor in; an unregistered producer's absence MUST NOT pin SKUs conservatively-referenced; membership MUST be a governed, audited change snapshotted symmetrically with the freeze-participant set
- **And** onboarding a new producer MUST NOT retroactively flip historical mutability/retirement decisions

**44. Grandfathered-snapshot retention coupling**
- **Given** a `catalogVersionId` referenced by ≥ 1 live grandfathered reference
- **When** retention/erasure would expire, tier, or GC that snapshot
- **Then** the snapshot MUST remain byte-identically resolvable for as long as a live reference exists, regardless of the statutory-max clock; retention expiry MUST be gated on no live references to that `catalogVersionId`
- **And** version-liveness MUST be sourced from per-version freeze-registration records (or a `(catalogVersionId, producer)` contract), never the SKU-level count alone; a process that would orphan a live reference MUST fail closed with an alert

**45. Pre-publish lint report**
- **Given** the `validate(lint)` operation before `CatalogVersion` publish
- **When** an admin runs it (or publish triggers it)
- **Then** the lint MUST return a structured, per-entity report of every override-requiring/attention condition (uncomposed bundles, missing default-locale attribute values, declarations against a `deprecated` unit) so the two-person override is informed and the audit records what was overridden

### Non-Functional Requirements (Show-Stoppers)

**1. Read latency**
- **Given** a warm read model holding 10K SKUs/tenant with ≥ 100 concurrent readers
- **When** browse/search reads execute within a tenant partition
- **Then** p95 latency MUST be < 100 ms over a 5-minute window, sustained via cache-first read models and partitioning

**2. Read throughput**
- **Given** the cache-first read model under load
- **When** browse/search traffic peaks
- **Then** the system MUST sustain ≥ 2,000 read QPS per tenant partition at the AC-1 latency target

**3. Publication propagation**
- **Given** an approved publish
- **When** the publish completes
- **Then** downstream event availability (incl. fan-out) MUST occur within < 3 s — distinct from read-model convergence (< 2 s) and the end-to-end posting-safe budget (< 5 s)

**4. End-to-end posting-safe budget**
- **Given** a write commit that must become safe for Contracts/Billing to post against
- **When** measured from commit to "posting-safe" (read converged AND all `freezeComplete` acks)
- **Then** the composite MUST be p99 < 5 s; if the freeze times out the version MUST remain non-posting-safe (fail closed)

**5. Snapshot archival & cold-resolution SLA**
- **Given** accumulating immutable snapshots under statutory-bounded retention at ≥ 10K SKUs/tenant
- **When** an archived ("cold") `catalogVersionId` is re-resolved
- **Then** re-resolution MUST remain byte-identical and meet a looser-than-hot target (interim p95 < 2 s)
- **And** snapshots + version history are financial records with a durability class (≥ 11 nines / replicated), periodic restore verification, and a cross-region/DR posture with RPO/RTO

**6. Scale & extensibility limits**
- **Given** a large tenant
- **When** the catalog grows
- **Then** the system MUST support ≥ 10K SKUs/tenant without breaching read latency, within configured extensibility limits, and MUST bound tenant count, cardinality, and `CatalogVersion` growth

**7. Graceful degradation & staleness exposure**
- **Given** read load above the throughput ceiling or read-model lag above budget
- **When** the system serves browse/search
- **Then** it MUST shed/queue excess load without ever serving cross-scope or unpublished content, and MUST expose staleness via the same machine-readable `asOfCatalogVersion` signal (no silently-stale response)

**8. Determinism & integrity**
- **Given** the registry invariants
- **When** authoring/publish runs
- **Then** version immutability, taxonomy acyclicity, SKU identity uniqueness, and metering-unit validity MUST be enforced fail-closed, and posted-period snapshots MUST remain immutable

**9. Backward-compatible schema evolution**
- **Given** a consumer pinned to schema `vN`
- **When** the registry publishes a `vN+1` payload
- **Then** the consumer MUST deserialize it (new fields optional with defaults); a CI contract test MUST assert backward compatibility on every schema change

**10. Availability & audit completeness**
- **Given** the catalog service
- **When** measured over the SLO window
- **Then** the cache-first read path MUST meet 99.9% availability and the write/publish path 99.5%; write paths MUST be fully audited even during partial failures

## 13. Dependencies

| Dependency | Description | Criticality |
|------------|-------------|-------------|
| Tenant identity & hierarchy (OSS/AMS + IdP) | `tenantId`, brand/region claims, OrgTier projection targets, role claims (registry MUST NOT mutate tenant topology) | `p1` |
| Plan & Price Modeling | Consumes published SKU identity/type, metering-unit declaration, `PlanTier`, `CatalogVersion`; produces `SkuReferenceCount`, `freezeComplete` ack, bundle composition-completed signal | `p1` |
| Tariffs / Pricing Logic | Consumes published SKU refs + `CatalogVersion` (price resolution there) | `p1` |
| Rating & Charging | Consumes metering-unit declaration + published SKU refs | `p1` |
| OSS metering | Emits usage values (external); consumes the metering-unit declaration | `p1` |
| Subscriptions (lifecycle & entitlements) | Produces `SkuReferenceCount`; consumes SKU refs + `PlanTier` + `replacedBy` + `mustMigrateBy` (post-v1); owns live-subscription migration | `p1` |
| Contracts & Agreements | Produces `SkuReferenceCount` (incl. draft/quote refs per contract) + `freezeComplete` ack; consumes `CatalogVersion` snapshots for quotes | `p1` |
| Billing & Invoicing | Produces `freezeComplete` ack; consumes SKU refs + `CatalogVersion` (descriptors authored in plan-price, frozen into `CatalogVersion`) | `p1` |
| Marketplace & Vendor Portal | References published SKUs; vendor ops remain in the Marketplace PRD (§4.8) | `p2` |
| Presentation / Portals | Consumes catalog read models for browse/search cache warming | `p2` |
| Events & Audit (Common Core) | Shared event system: durable acceptance, per-consumer delivery/DLQ state, retry; transport owned there | `p1` |
| BSS Architecture Manifest | §4.1 (registry), §4.4 (posting immutability), §4.2/§4.3/§4.6/§4.8 (consumers), §2.1.3, §7.2 | `p1` |

> **Registry is upstream of all commercial modeling.** A SKU MUST be published before a Plan/Price can reference it. The registry MUST NOT require any downstream consumer to re-interpret mutable catalog state for **posted** periods; the `CatalogVersion` snapshot contract is authoritative (manifest §4.4).

## 14. Assumptions

- The `SkuReferenceCount` signal will be delivered by Subscriptions, Contracts, and plan-price with a committed owner + date (pre-approval gate); until then AC #2/#4/#18 run fail-safe, bounded by the break-glass path and the fail-safe tripwire.
- Interim configurable-policy defaults (§17.1) are enforceable at launch; each final value is owned by another function and changes are governed/audited.
- Numeric NFR targets are binding **design targets** until the NFR workshop (within 2 weeks of approval; DRI = BSS Program Lead).
- The shared event system (Common Core) provides ordering/at-least-once/DLQ transport; the registry states only its own emission/projection obligations.
- Recognized-set owners (metering units → Product + Rating; tax/GL codes → Finance; `PlanTier` → Product) seed and govern their sets; the registry validates against them.
- `PRD-plan-price-modeling` and this PRD stay consistent on the shared fields via a CI seam test; the combined predecessor PRD is refactored to Marketplace-only after approval.

## 15. Open Questions

> **Pre-approval gates (must be closed at approval).** Two items MUST carry a committed owner/decision **before** sign-off because they govern launch behavior: (1) **`SkuReferenceCount` owner + delivery date** — until committed, AC #2/#18 run fail-safe; (2) **Region-set algebra** for AC #5 overlap semantics. Event delivery resilience and `CatalogVersion` publish concurrency are build conditions captured as FRs/ACs.

| **Question** | **Answer** | **Date Answered** |
|--------------|------------|-------------------|
| **GATE — `SkuReferenceCount` signal owner + delivery date** | Contract + counting defined; owner sign-off + committed delivery date MUST be recorded at approval. Until it ships, AC #2/#18 run fail-safe (bounded by break-glass + tripwire). *(Owner: Architecture + Subscriptions/Contracts.)* | TBD (gate) |
| **GATE — Region-set algebra** (containment/wildcard/disjointness for AC #5) | Interim conservative rule live (non-disjoint ⇒ overlapping, fail-closed); exact algebra pinned in Design before implementation. *(Owner: Design + Product.)* | TBD (gate) |
| Contracts draft/quote references: count toward `referenced`? + re-resolve-at-freeze behavior | Producer contract must declare both, identical across AC #2/#4/#18; recorded at sign-off. *(Owner: Contracts + Architecture.)* | TBD |
| EOL `mustMigrateBy`: pull into v1 or confirm the post-v1 deferral? | Registry side deferred; needs a date to pull in or confirmation. Gates EOL child cascade. *(Owner: Subscriptions.)* | TBD |
| Finance materiality threshold production value + date | Dimension + interim default resolved; needs a committed production value + date. *(Owner: Finance.)* | TBD |
| Legal content-PII prohibition sign-off | Normative position = hard prohibition, no carve-out; Legal to confirm sufficiency + detector posture, recorded at approval. *(Owner: Legal.)* | TBD |
| Data retention durations per record class + PII pseudonymization age | Interim set (financial/version/audit → statutory max). Final durations per jurisdiction. *(Owner: Legal/Finance.)* | TBD |
| Recognized metering-unit set owner + add-unit / de-list workflow | Interim seed ships; de-list governed. Owner + workflow. *(Owner: Product + Rating.)* | TBD |
| Recognized tax-category / GL-code sets: owner + add / de-list workflow | Interim configured sets ship; validated + required at publish for product/service types. Owner + workflow. *(Owner: Finance.)* | TBD |
| PlanTier taxonomy governance ownership confirmation | Taxonomy + SKU value owned here; plan enforces presence at plan publish. Confirm with plan-price. *(Owner: Product + plan-price.)* | TBD |
| Catalog taxonomy/category scheme (IaaS/PaaS/SaaS/…) | To be defined. *(Owner: Product.)* | TBD |
| Media/binary asset ownership (does the registry hold asset URIs?) | Out of scope as binaries; registry may carry URIs. Confirm owner — Presentation / Marketplace / DAM. *(Owner: Product + Presentation/Marketplace.)* | TBD |
| Catalog-relationship block beyond `bundle`/parent-child/`replacedBy` | Registry owns `replacedBy`/supersedes; other types out of scope v1. Confirm need vs plan-price/Subscriptions. *(Owner: Product + Architecture.)* | TBD |
| Per-seat monetization: truly zero registry footprint? | Confirm no seat-as-unit artifact; metered seats would collide with the single-unit rule. *(Owner: Product.)* | TBD |
| Monetization-model coverage: plan-price pointer accepted in lieu of registry fields? | Traceability map provided (§17.2). *(Owner: Finance/Program.)* | TBD |
| Event-bus transport contract owner + home (ordering, at-least-once, DLQ retention) | Registry states its requirements; the contract is owned by Common Core / Events & Audit. *(Owner: Architecture/Eng — Common Core.)* | TBD |
| Event-log retention/TTL value | MUST be ≥ the bootstrap gap implied by AC #29. *(Owner: Eng/Common Core.)* | TBD |
| `CatalogVersion` archival economics: storage growth + publishes/day/tenant target | Tiering allowed while byte-identical; needs the publish-frequency target before storage design. *(Owner: Eng/Finance.)* | TBD |
| Snapshot durability / DR targets (RPO/RTO + restore-verification cadence) | Snapshots are financial records: interim ≥ 11 nines + periodic checksum restore verification; RPO/RTO ratified at the NFR workshop. *(Owner: Eng/Program.)* | TBD |
| Snapshot-GC version-liveness source | Per-SKU count has no version dimension; source from per-version freeze-registration or a `(catalogVersionId, producer)` contract. Confirm. *(Owner: Architecture + freeze participants.)* | TBD |
| Cross-PRD seam contract-suite owner + repo/pipeline | Proposed: BSS Catalog/Architecture in `api-contracts` CI. Final owner sign-off. *(Owner: needs assignment.)* | TBD |
| Disposition of `PRD-product-catalog-marketplace-202601120119` (refactor to Marketplace-only) | Keep now; refactor to §4.8-only after this + plan-price + Tariffs approved. *(Owner: Product/Program.)* | TBD |
| NFR workshop: named DRI, held within 2 weeks of approval, SLO table ratified | Targets are binding design targets until then. *(Owner: BSS Program Lead.)* | TBD |

## 16. Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| `SkuReferenceCount` signal slips | AC #2/#4/#18 stuck in fail-safe (no immutable-field correction on referenced SKUs) | Pre-approval gate on owner + date; break-glass bounds per-operation debt; the fail-safe tripwire (> 5/30d) bounds it in time |
| Region-set algebra undefined | False name collisions/allows on create (AC #5) | Interim conservative fail-closed rule; pin the algebra in Design before implementation (gate) |
| Snapshot lost/corrupted | Breaks byte-identical reproducibility for every contract frozen to it — a compliance event | Snapshots as financial records: ≥ 11 nines durability, periodic checksum restore verification, retention gated on live references (AC #44/#55) |
| Registry ↔ plan-price schema drift | Silent divergence breaks downstream binding/posting | Shared schema pin + CI seam contract test that fails closed (AC #36/#48) |
| Stuck cross-module freeze | Posting blocked or unsafe | `freezeComplete` fail-closed + idempotent re-trigger + governed force-completion (AC #21/#22) |
| Full-snapshot-per-publish cost growth | `CatalogVersion` storage/economics at 10K+ SKUs × frequent publishes | Batching-as-policy; publishes/day/tenant target + archival economics at the NFR workshop (AC #44/#45) |
| Combined predecessor PRD left authoritative for registry | Duplicate/divergent catalog requirements | Refactor `PRD-product-catalog-marketplace` to Marketplace-only after approval (§15) |

## 17. Reference Materials

| **Material** | **Link** | **Comments** |
|--------------|----------|--------------|
| BSS Architecture Manifest | `docs/bss/manifest/vz-arch-manifest-bss-only.md` | §4.1 (registry) incl. the Decomposition (BSS realization) note = normative home of the §4.1 split; §4.4 posting immutability; §4.2/§4.3/§4.6/§4.8 consumers; §2.1.3; §7.2 events |
| Plan & Price Modeling | `docs/bss/prd/PRD-plan-price-modeling-202605281200/PRD-plan-price-modeling-202605281200.md` | Owns Plan/Price/PriceWindow/PriceList/Bundle composition/add-ons/billing descriptors/plan lifecycle; builds on this registry |
| Rating (evaluation core) | `gears/bss/rating/docs/PRD.md` | Price evaluation over the primitives plan-price authors |
| Rating (pipeline) | `gears/bss/rating/docs/PRD.md` | Consumer of metering-unit declaration and published SKU refs |
| Subscriptions — Lifecycle | `docs/bss/prd/PRD-subscriptions-lifecycle-202604021200/PRD-subscriptions-lifecycle-202604021200.md` | Consumes SKU refs + PlanTier + CatalogVersion; owns live-subscription migration |
| Product Catalog & Marketplace (predecessor) | `docs/bss/prd/PRD-product-catalog-marketplace-202601120119/PRD-product-catalog-marketplace-202601120119.md` | Combined §4.1+§4.8 predecessor; registry scope superseded here, Marketplace retained there |
| Project glossary | `docs/project-glossary.md` | Canonical terms |
| Trace chain | `AGENTS.md` (repository root) | Manifest → PRD → ADR → Design → Stories |

### 17.1 Configurable-Policy Interim Defaults

| Policy | Interim default (fail-safe) | Final owner |
|--------|-----------------------------|-------------|
| Materiality threshold | Two-person rule on any material-field change (always); affected-entity-count trigger ≥ 10; single-entity non-material change passes with one approver | Finance |
| Recognized tax-category & GL-code sets | Configured enum + GL chart; unknown codes rejected at authoring; new codes require elevated approval; de-list blocked while referenced; required at publish for product/service types | Finance |
| PlanTier taxonomy seed | Seeded with a neutral value (`standard`/`none`) + operator-defined tiers; tier identity is a stable code, rename = display-only | Product |
| Idempotency-key retention | ≥ 24h and ≥ the maximum freeze timeout | Eng/Design |
| `SkuReferenceCount` freshness threshold | 15 min; staler → conservative + alert | Architecture |
| Fail-safe break-glass tripwire | > 5 break-glass corrections / 30 days → escalate + signal delivery becomes release blocker | Architecture |
| Retirement / EOL lead-time | ≥ 30 days between event and effective hide | Subscriptions + Product |
| Recognized metering-unit set | Seeded with platform base units (`vCPU-hours`, `GB-storage`, `GB-egress`, `request-count`); new units require elevated approval; de-list blocked while referenced | Product + Rating |
| Retention — financial/version/audit | Retain to statutory maximum (not "indefinite") | Legal/Finance |
| Retention — operator PII | Pseudonymize at erasure request or a defined max age, whichever first | Legal/Finance |
| Read-model convergence | p99 < 2 s after write commit | Eng |
| Event propagation / fan-out | p99 < 3 s after publish | Eng |
| End-to-end posting-safe budget | p99 < 5 s (read converged and `freezeComplete`) | Eng |
| Cold `catalogVersionId` resolution | p95 < 2 s (looser than hot reads) | Eng |
| Snapshot durability & DR / RPO-RTO | ≥ 11 nines / replicated storage; periodic restore verification; RPO/RTO at the NFR workshop | Eng/Program |
| Numeric NFRs | Binding design targets until the NFR workshop (approval + 2 weeks); DRI = BSS Program Lead | Program/Eng |

### 17.2 Monetization-Model Traceability

| Monetization model | Where authored / evaluated |
|--------------------|----------------------------|
| flat, per-seat, tiered, volume, hybrid, commitment | `PRD-plan-price-modeling-202605281200` (authoring) + `PRD-tariffs-pricing-logic-202604011200` (evaluation) |
| usage | Metering-unit **declaration** here (registry) + plan-level meter binding (plan-price) + rating (Rating) |

Absence of a monetization-model marker on a SKU is **intentional**, not a missing field.

---

*Child artifacts: ADR(s) for versioning/snapshot strategy and lifecycle/deprecation modeling; the gear's DESIGN (`gears/bss/products/docs/DESIGN.md`, pending) for entity schemas, APIs, events, and read-model design; STORY documents per scope item. The §4.1 registry↔commercial decomposition is recorded in the manifest §4.1 Decomposition (BSS realization) note, not a separate ADR.*
