<!-- CONFLUENCE_TITLE: [BSS]: Plan & Price Modeling — Design Set -->
<!-- Related: ../DESIGN.md, ../PRD.md, ../ADR/ | Owners: BSS Product Catalog team -->

# Plan & Price Modeling — Design Set

<!-- toc -->

- [Slice documents](#slice-documents)
- [Slice map (PRD §6 ↔ implementation phase)](#slice-map-prd-6--implementation-phase)

<!-- /toc -->

This folder holds the Plan & Price Modeling technical design as a **set of slice designs**: a
shared **Catalog Foundation** ([`01-foundation.md`](./01-foundation.md)) plus per-capability
handler designs. Every slice **publishes through** the Foundation — the `Plan`/`Price` entity
model, the canonical scope key, the draft→publish state machine, the fail-closed validation
pipeline, append-only versioning/supersession, the read-model projection + `pricingSnapshotRef`
contract, and the frozen event fan-out. The Foundation owns no capability policy (it does not
know what a billing cycle or model kind is); each slice is a handler that authors draft state,
registers its validation rules and read-model fields, and calls the Foundation publish API.

**The canonical index for this set — architecture overview, the phased slice map, dependency
order, cross-cutting normative statements, the ADR index, and traceability — is
[`../DESIGN.md`](../DESIGN.md).** Requirements (WHAT/WHY) live in [`../PRD.md`](../PRD.md);
decision rationale in [`../ADR/`](../ADR/).

## Slice documents

The cross-slice [UI read-contract amendment](./ui-read-contracts.md) records the
2026-09-10 implementation and remaining decisions for slices 02, 04 and 05.

- [`01-foundation.md`](./01-foundation.md) — **shared engine**: `Plan`/`Price` model, canonical scope key, draft→publish state machine, fail-closed validation pipeline, append-only history + versioning/supersession, read-model projection + `pricingSnapshotRef`, event fan-out + `CatalogVersion` request, tenant isolation, ISO 4217 money, idempotency/ETag. Carries the catalog-wide normative statements (§4).
- [`02-plan-definition.md`](./02-plan-definition.md) — billing cycles, custom frequency, per-seat quantity provenance (`quantitySource` persisted/validated in Slice 3), one-time-setup row, mandatory `PlanTier`, meter injectivity, add-on rules, phases + `convertsToPhaseId`, billing descriptors (PRD §6.1, §6.3)
- [`03-price-structure.md`](./03-price-structure.md) — explicit `modelKind`, tier-band validation, `package` pricing, evaluation-policy placement, level-based aggregation (`aggregationFunction`/`aggregationGranularity`/`maxHoldGranules`, D-44), joint golden-fixture conformance gate (PRD §6.2)
- [`04-currency-tax.md`](./04-currency-tax.md) — per-`(currency, region)` rows, region/brand taxonomies, tax-display basis + `not_sellable_ga` gate, single-currency-per-invoice binding, base-price preview (PRD §6.4)
- [`05-governance.md`](./05-governance.md) — materiality + two-person rule, per-currency threshold policy, RBAC deny-by-default + the preview grant (the backdating grant and its flow are struck — D-330), tenant/region isolation, audit trail + retention (PRD §6.7 approval, §6.12)
- [`06-consumer-contracts.md`](./06-consumer-contracts.md) — proration input contract (canonical `prorationBasis` enum), `billingTiming`, entitlement grant set, plan-change contract, rating compatibility (PRD §6.9)
- [`07-pricewindow-linkage.md`](./07-pricewindow-linkage.md) — `PriceWindow` ownership (store, state machine, activation job, `PriceWindow*` events — consolidated per D-03), window coverage + future-gap, sellability surface (joint gate), grandfathering eligibility + atomic cutover (PRD §6.5)
- [`08-bundles.md`](./08-bundles.md) — bundle price basis (`sum_of_parts` via component `planId`s / `own_price`), currency + frequency coverage, rev-share reconciliation, itemization (PRD §6.3 bundle)
- [`09-price-overlays.md`](./09-price-overlays.md) — `PriceOverlay` authoring/validation (scope, adjustment, explicit precedence, tax basis) + `customerGroup` taxonomy, effective-dated audited membership, resolved-group freezing (PRD §6.6)
- [`10-advanced-primitives.md`](./10-advanced-primitives.md) — reserved capacity (same-row attributes), prepaid grant (GA-gated), `includedAllowance` compilation (D-45), trailing-tier qualification (`tierQualificationWindow`, D-40), derived meter formula-as-data, `discountRef` hook, typed `minQtyThreshold` (PRD §6.10)
- [`11-lifecycle.md`](./11-lifecycle.md) — retirement (window-cancellation trigger + operator warning), scheduled migration + `PlanLink` (idempotent, cancellable), safety deltas + contract-lock exclusion, `migrated-origin` snapshot synthesis (PRD §6.8)
- [`12-operator-efficiency.md`](./12-operator-efficiency.md) — clone (eligibility resets), bulk import (validate-all / commit-per-row), mass repricing (journaled idempotency, event dedup, version coalescing), history + export (PRD §6.11)

The design set is **complete**: all 12 slices are authored. See
[`../DESIGN.md` §1.3](../DESIGN.md#13-architecture-layers) for phases and dependencies.

## Slice map (PRD §6 ↔ implementation phase)

The numeric prefix is **implementation order**, not the PRD §6 subsection number — the two
axes deliberately do not line up (a slice is scoped by PRD decomposition but built when its
dependencies exist).

| Doc | PRD §6 | Phase | Depends on |
|-----|--------|-------|------------|
| `01-foundation` | 6.2/6.7 core, §17.4/17.5 | 0/1 | — |
| `02-plan-definition` | 6.1, 6.3 | 1 | 01 |
| `03-price-structure` | 6.2 | 1 | 01, 02 |
| `04-currency-tax` | 6.4 | 1/2 | 01, 02 |
| `05-governance` | 6.7, 6.12 | 1/2 | 01 |
| `06-consumer-contracts` | 6.9 | 2 | 01, 02, 03 |
| `07-pricewindow-linkage` | 6.5 | 2 | 01, 02, 03, 05 |
| `08-bundles` | 6.3 (bundle) | 2/3 | 02, 03, 04 |
| `09-price-overlays` | 6.6 | 3 | 01, 04, 05 |
| `10-advanced-primitives` | 6.10 | 3 | 02, 03, 05 |
| `11-lifecycle` | 6.8 | 3/4 | 05, 07 |
| `12-operator-efficiency` | 6.11 | 4 | all |

05-governance additionally gates **every** slice's publish path (approval + authz); the
column lists it only where a slice depends on 05 beyond that universal gate (cutover
approval unit, membership materiality, grant-price materiality).

See [`../DESIGN.md` §1.3](../DESIGN.md#13-architecture-layers) for the dependency graph and
phase rationale.
