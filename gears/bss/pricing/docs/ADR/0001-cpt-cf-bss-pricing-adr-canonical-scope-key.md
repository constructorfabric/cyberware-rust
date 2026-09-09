---
status: accepted
date: 2026-07-09
decision-makers: "BSS Product Catalog team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0001: Canonical Price-Row Scope Key — Extend the Manifest Key Additively

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Keep the manifest key unchanged](#keep-the-manifest-key-unchanged)
  - [Model components/eligibility as separate plans or rows outside the key](#model-componentseligibility-as-separate-plans-or-rows-outside-the-key)
  - [Extend the manifest key additively (chosen)](#extend-the-manifest-key-additively-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-pricing-adr-canonical-scope-key`

> **Amended by ADR-0002 (2026-07-10)**: the key gains an eighth additive axis — `cohort`, the
> grandfathering generation discriminator (`cpt-cf-bss-pricing-adr-grandfathering-cohort-axis`).
> The additive-extension decision below is unchanged; seven-column statements read as
> eight-column after the amendment.

## Context and Problem Statement

Several catalog invariants hang off "the price row's identity": row-uniqueness (no duplicate
rows), supersession (replace the active row within an identity), `PriceWindow` non-overlap (no
two active windows for one identity), and publish-time window coverage. The BSS Architecture
Manifest §4.1/§8.2 and the effective-dating price-windows use case define this identity as
`(plan, currency, region, priceOverlay)`.

That key is too narrow for the commercial shapes this PRD MUST support on a single `planId`:

- A **hybrid plan** carries a `recurring` base row **and** a `usage` row (optionally a `one_time_setup` row) at the same time — under the manifest key these collide as "duplicates" of one identity.
- A **one-time plan's** base row and a recurring/hybrid plan's setup row are different charge components that must not collide.
- **Grandfathering** must keep a legacy price live for pre-cutover subscribers while a new price serves new subscribers — two prices that are active **concurrently** for what the manifest key would call one identity, which reads as a non-overlap violation.
- **Plan phases** (trial/intro/evergreen) each carry their own price schedule for one plan.

What is the canonical identity of a price row, such that all four invariants hold without
banning legitimate multi-component / grandfathered / phased plans?

## Decision Drivers

* The four invariants (uniqueness, supersession, non-overlap, coverage) must key off **one** identity, used identically by the catalog and by Tariffs' non-overlap check.
* A hybrid plan's components, a one-time base vs a setup charge, a grandfathered row vs its successor, and per-phase schedules must be **distinct** identities, not duplicates or overlaps.
* The key must not contradict the manifest §4.1/§8.2 non-overlap invariant — it must be a compatible extension, not a fork.
* `brand` must stay **out** of the price-row identity (the manifest `Price` model has no `brand` field; brand pricing is a brand-scoped `PriceOverlay`).
* The identity must reconcile "grandfathered rows are live-resolved" with "published rows are immutable and consumers read frozen snapshots".

## Considered Options

* Keep the manifest key `(plan, currency, region, priceOverlay)` unchanged and disallow the colliding shapes.
* Model components / eligibility / phases as separate plans or as rows disambiguated *outside* the key (e.g. a row "type" column not part of uniqueness).
* Extend the manifest key **additively** with `phase`, `priceEligibility`, `chargeKind` (chosen).

## Decision Outcome

Chosen option: **extend the manifest key additively**. The single canonical scope key for
row-uniqueness, supersession, `PriceWindow` non-overlap, and window coverage is:

```text
(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind)   -- + cohort (ADR-0002)
```

with axis defaults `priceOverlay = base`, `phase =` the plan's terminal `phase_id` (implicit terminal phase auto-created for non-phased plans; id-typed axis — D-19),
`priceEligibility = all_subscriptions`, and `chargeKind` per row
(`recurring | usage | one_time | one_time_setup`). The manifest §4.1/§8.2 non-overlap
invariant and the Tariffs non-overlap key are aligned to this key; the effective-dating use
case's narrower `(plan, currency, region, priceOverlay)` key is **superseded** by this canonical
key for normative purposes. `brand` is **not** an axis — brand-differentiated pricing is a
brand-scoped `PriceOverlay`.

Chosen because it makes every legitimate shape a **distinct key** while preserving the
non-overlap invariant exactly: a hybrid plan's `recurring`/`usage`/`one_time_setup` rows differ
in `chargeKind`; a grandfathered row and its successor differ in `priceEligibility` and so hold
concurrent active windows **without** violating non-overlap; per-phase schedules differ in
`phase`. It is additive, so it does not fork the manifest — the pre-existing axes keep their
meaning and the new axes carry safe defaults for plans that do not use them.

### Consequences

* Row-uniqueness enforcement (a partial `UNIQUE` index over **current** rows — published and not superseded) is on the **seven-column** key (eight with `cohort`, ADR-0002), holding at most one current row per key; **temporal `PriceWindow` non-overlap and coverage are enforced by publish-time validation (Slice 7) + the effective-dating UC (the UC enforcement partner was later absorbed into Slice 7 by ADR-0003), not by the index** (a published predecessor and its scheduled successor legally coexist). The Foundation's `ScopeKey` component constructs and defaults the key centrally (Foundation §4.1).
* Grandfathering reconciles with the frozen-snapshot doctrine: an `existing_grandfathered` row is a **distinct, immutable** key that Tariffs live-resolves; the cutover shortens the current `all_subscriptions` window and schedules the grandfathered copy + successor as one atomic unit, so no coverage gap opens (Foundation §4.3).
* Supersession is explicitly **scoped to one canonical key** and operates within one `priceEligibility` class and one `chargeKind`; it opens/closes a `PriceWindow` rather than overlapping it.
* Tariffs MUST adopt the identical key for its non-overlap check; divergence would re-introduce the collisions this decision removes. Cross-team alignment with Tariffs is required.
* Publish-time coverage resolves on the **base** `priceOverlay`; partner/orgTier/brand overlays are separate `PriceOverlay` rows applied at evaluation, not part of the base coverage check.
* A wider uniqueness index is a modest storage/write cost versus the narrower manifest key — accepted.

### Confirmation

* Design review: the Foundation `ScopeKey` component, the `pricing_price` partial `UNIQUE` index (current rows), and the Slice 7 window non-overlap/coverage validation all key off the same seven columns (eight with `cohort`, ADR-0002); no invariant keys off a narrower subset.
* Integration test: a hybrid plan (`recurring` + `usage` + `one_time_setup`) publishes without a duplicate-scope failure; a grandfathered row and its successor hold concurrent active windows without a non-overlap violation; a per-phase schedule publishes per phase.
* Cross-team checkpoint: Tariffs confirms it evaluates the non-overlap check on the identical seven-column key (eight with `cohort`, ADR-0002).

## Pros and Cons of the Options

### Keep the manifest key unchanged

Retain `(plan, currency, region, priceOverlay)` and forbid the colliding shapes.

* Good, because zero change to the manifest key and the effective-dating use case.
* Bad, because it bans hybrid plans, one-time-setup-on-recurring, per-phase schedules, and time-bound grandfathering on one `planId` — all in-scope PRD requirements.
* Bad, because operators would clone SKUs to work around it, defeating the lifecycle-safety goal.

### Model components/eligibility as separate plans or rows outside the key

Disambiguate with a non-key "type" column, or split components/eligibility across multiple plans.

* Good, because the manifest key text is untouched.
* Bad, because uniqueness/non-overlap then key off an identity that does not match the real row identity — collisions still occur or must be special-cased per shape.
* Bad, because separate plans fragment one commercial offer across many `planId`s, breaking hybrid co-resolution and the plan-change contract.
* Bad, because a non-key type column is exactly a key axis in disguise, but without the enforcement — the worst of both.

### Extend the manifest key additively (chosen)

Add `phase`, `priceEligibility`, `chargeKind` with safe defaults.

* Good, because every legitimate shape is a distinct key and the non-overlap invariant holds by construction.
* Good, because it is additive with defaults — plans that do not use the new axes behave exactly as under the manifest key.
* Good, because it reconciles live-resolved grandfathering with immutable published rows (distinct `priceEligibility` key + immutable row).
* Neutral, because it requires Tariffs to adopt the identical key (a coordinated but one-time alignment).
* Bad, because the uniqueness index widens and the effective-dating use case's narrower key is superseded (a documented normative change, not a silent one).

## More Information

Normative catalog-wide statement: [`design/01-foundation.md` §4.1](../design/01-foundation.md#41-canonical-scope-key-normative).
PRD source: [`PRD.md`](../PRD.md) §2.2 (Canonical Scope Key) and §17.5 (Price-Change Mechanisms).
The PRD explicitly names this ADR as a planned child artifact ([`PRD.md`](../PRD.md) §17.8 closing note).

## Traceability

- **PRD**: [`PRD.md`](../PRD.md)
- **DESIGN**: [`design/01-foundation.md`](../design/01-foundation.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-bss-pricing-fr-supersession` — supersession is scoped to one canonical key, opening/closing a `PriceWindow` within one `priceEligibility`/`chargeKind`
* `cpt-cf-bss-pricing-fr-published-rows-append-only` / `cpt-cf-bss-pricing-fr-plan-versioning` — a change writes a new immutable row on a distinct key rather than mutating in place
* `cpt-cf-bss-pricing-fr-hybrid-completeness` — a hybrid plan's `recurring` and `usage` components are distinct keys on one `planId`
* `cpt-cf-bss-pricing-fr-one-time-setup` — a `one_time_setup` charge is a distinct key, not a duplicate of the base row
* `cpt-cf-bss-pricing-fr-plan-phases` — per-phase price schedules are distinct keys via the `phase` axis
* `cpt-cf-bss-pricing-fr-region-brand-taxonomy` — `brand` is not a price-row axis; brand pricing is a brand-scoped `PriceOverlay`
* `cpt-cf-bss-pricing-constraint-canonical-scope-key` — the DESIGN constraint this ADR grounds
