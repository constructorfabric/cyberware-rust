---
status: accepted
date: 2026-07-10
decision-makers: "BSS Product Catalog team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0002: Multi-Generation Grandfathering — the `cohort` Scope-Key Axis

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Normative one-generation-per-key limit](#normative-one-generation-per-key-limit)
  - [Full per-subscription price binding (Stripe model)](#full-per-subscription-price-binding-stripe-model)
  - [Additive `cohort` axis + snapshot-derived selection (chosen)](#additive-cohort-axis--snapshot-derived-selection-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis`

## Context and Problem Statement

Under ADR-0001 a grandfathering cutover schedules the legacy-price copy on the single
`priceEligibility = existing_grandfathered` value of the scope key. The first cutover
therefore **permanently occupies** the grandfathered key: a second cutover on the same
remaining axes (the routine annual reprice where each cohort keeps its signed price) must
schedule a second copy on the identical key — a non-overlap violation — and every escape
hatch is forbidden (a grandfathered row MUST NOT be superseded; its window MUST NOT be
shortened below `grandfatherUntil`). The design silently supported exactly **one grandfathered
generation per key** while the PRD never stated that limit (review finding D-02).

Multi-cohort retention is table stakes in comparable systems: Stripe/Chargebee/Recurly get N
generations implicitly via per-subscription price binding; Kill Bill gets them catalog-side
via versioned catalogs + `effectiveDateForExistingSubscriptions`. Product direction confirmed
multi-generation grandfathering as a required capability **now**, not a future option.

## Decision Drivers

* The routine commercial flow — repeated repricing with each cohort keeping its price — must be expressible on one `planId` without plan cloning or cohort migration.
* Renewal semantics stay **default-move** (`fr-supersession`: a supersession moves the base at next renewal; grandfathering remains the explicit exception) — a wholesale switch to per-subscription binding would invert the default and make the common case (reprice moves everyone) expensive.
* The catalog stays the deterministic source for "what does cohort X pay and why" (append-only rows, windows, audit, mass-repricing exclusions must see every generation as a first-class row).
* All ADR-0001 invariants (uniqueness, supersession scoping, window non-overlap, coverage) must keep holding per identity, with Tariffs evaluating the identical key.
* The extension must be additive — safe defaults for every existing row shape, no manifest fork (same discipline as ADR-0001).

## Considered Options

* Keep one generation per key, make the limit normative (reject the second cutover with an error; operator path = tighten `grandfatherUntil`, then cut over).
* Full per-subscription price binding (Stripe model): renewal resolves the subscription's pinned price id; eligibility classes dissolve.
* Extend the canonical scope key **additively** with a `cohort` axis; select among generations by the subscription's pinned snapshot (chosen).

## Decision Outcome

Chosen option: **additive `cohort` axis + snapshot-derived selection**. The canonical scope
key becomes:

```text
(planId, currency, region, priceOverlay, phase, priceEligibility, chargeKind, cohort)
```

* `cohort` is the **grandfathering generation discriminator**: the UTC cutover instant that created the generation. Default `cohort = none` on every non-grandfathered row; publish validation enforces `cohort ≠ none ⇔ priceEligibility = existing_grandfathered`. It is unrelated to `customerGroup` segment pricing.
* **Every cutover creates a new generation**: the copy lands on `(… , existing_grandfathered, chargeKind, cohort = T)`; prior generations' rows and windows are untouched (still immutable, still live-resolvable). The successor stays `(… , all_subscriptions, chargeKind, none)`. A cutover whose instant equals an existing generation's `cohort` is a duplicate key — rejected at compose (`DUPLICATE_SCOPE_KEY`).
* **Selection among generations is by subscription binding, not by class specificity**: most-specific-wins (W3) orders eligibility *classes* only; within `existing_grandfathered`, Tariffs resolves the row whose `cohort` equals the cohort of the subscription's **pinned price id** (`pricingSnapshotRef` already pins resolved price ids — no new binding store). New subscriptions never bind grandfathered rows (unchanged). At a generation's `grandfatherUntil` expiry, that cohort's subscriptions re-bind at next renewal to the current eligible row (unchanged mechanics, now per generation).
* **Bootstrap — a pin carrying `cohort = none` (amended 2026-08-01, D-126):** the rule above closes only from the *second* cutover onward. A subscription that predates a key's **first** cutover is pinned to a non-grandfathered row, whose `cohort` is `none` by construction (`cohort ≠ none ⇔ existing_grandfathered`), so it matched no generation at all — while the class filter (`activatedAt` before cutover) had already excluded the `all_subscriptions` successor, leaving the exact population this ADR exists to protect resolving nothing and failing closed at the changeover. Therefore: **when the pinned price id's `cohort` is `none`, the bound generation is the one whose `cohort` equals the pinned row's window `effectiveTo`** — by construction of the cutover (which shortens the predecessor window *to* the cutover instant) that is the generation created by the cutover that closed the pinned row, and the input is already pinned and projected, so no new store appears. **If no generation carries that instant**, the pinned row was closed by a supersession rather than a cutover: the `existing_grandfathered` class contributes **no** candidate and resolution continues down the class order to the `all_subscriptions` row — the correct "superseded, not grandfathered" outcome, and the clause that keeps an empty class from failing closed. From the first renewal after the cutover the subscription is re-pinned to its generation and the primary rule applies unchanged. Tariffs adopts this alongside the class-then-cohort selection.
* Uniqueness, supersession scoping, window non-overlap, and coverage all key off the **eight-column** key; each generation carries its own window and its own `grandfatherUntil`.

### Consequences

* The D-02 impossibility dissolves: sequential cutovers produce coexisting generations (`$100` cohort, `$120` cohort, current `$140`), each a distinct key with concurrent active windows and no non-overlap violation.
* Tariffs MUST adopt the eight-column key for its non-overlap check and the **binding-based within-class selection** (class matching first, then `cohort` from the pinned price id) — cross-team contract update (PRD §9.2).
* Subscriptions is unchanged except that re-bind at `grandfatherUntil` expiry is per generation (its binding/dedup mechanics already operate per subscription).
* The partial `UNIQUE` index (current rows), the Foundation `ScopeKey` component, supersession ("within one `priceEligibility` class, one `cohort`, one `chargeKind`"), Slice 7 coverage/gap checks, mass-repricing exclusions, and clone's "grandfathered rows are not cloned" all extend mechanically to the new axis.
* Unbounded generations are a plan-size concern: the generation count per key participates in the plan/tier size caps (provisional NFR, PRD §14) — a soft cap with a publish warning, not a hard product limit.
* A modest widening of the uniqueness index and every key-carrying surface (read model, events' aggregate identity untouched — `cohort` is a row attribute inside the plan aggregate) — accepted.

### Confirmation

* Design review: `ScopeKey`, the partial `UNIQUE` index, Slice 7 validation, and the Tariffs non-overlap check all key off the same **eight** columns; `cohort ≠ none ⇔ existing_grandfathered` is publish-enforced.
* Integration test: two sequential cutovers on one remaining-axes identity yield three concurrently-active rows (two generations + successor); each cohort's subscription resolves its own generation's price; expiry of generation 1's `grandfatherUntil` re-binds only that cohort.
* Integration test (**bootstrap, D-126**): at the **first** cutover, a subscription still pinned to the predecessor (`cohort = none`) resolves the new generation — matched by the pinned row's window `effectiveTo` — and not the `all_subscriptions` successor; a subscription whose pinned row's window was closed by a **supersession** resolves the `all_subscriptions` row through the class order rather than failing closed.
* Cross-team checkpoint: Tariffs confirms class-then-cohort resolution and the eight-column non-overlap key; Subscriptions confirms per-generation re-bind.

## Pros and Cons of the Options

### Normative one-generation-per-key limit

Reject the second cutover (`CUTOVER_GRANDFATHERED_OCCUPIED`) unless the prior generation's `grandfatherUntil` precedes the new instant.

* Good, because it is honest about the current mechanics at ~zero design cost.
* Bad, because the routine annual-reprice-with-retention scenario — the primary reason grandfathering exists — is impossible on one plan; operators fall back to plan cloning, defeating lifecycle safety.
* Bad, because lifting the limit later means changing the scope key **after** launch — a migration of append-only history, snapshots, and the Tariffs contract; the most expensive possible time to do it.

### Full per-subscription price binding (Stripe model)

The subscription's pinned price id is the resolution source at renewal; eligibility classes dissolve.

* Good, because grandfathering (any depth) is free and D-02/D-03/D-04/D-05 all dissolve.
* Bad, because it inverts renewal semantics to **default-stay**: every subscriber is implicitly grandfathered, and the common case — a supersession moving the whole base at renewal — becomes a mass per-subscription re-bind operation (the documented pain of that model).
* Bad, because pricing truth relocates into subscription state: the catalog alone can no longer answer "who pays what at `t`" (audit, price history, repricing surfaces fragment), and the ratified Tariffs step-2 contract plus the window/coverage safety net would be reworked wholesale.

### Additive `cohort` axis + snapshot-derived selection (chosen)

* Good, because each generation is a first-class catalog row (windows, audit, history, exclusions all apply) — the Kill Bill-style catalog answer — while cohort membership rides the **already existing** snapshot pin — the Stripe-style pointer, exactly where default-stay is wanted.
* Good, because it is additive with safe defaults (`cohort = none`), repeating the proven ADR-0001 move; nothing changes for non-grandfathered rows.
* Neutral, because Tariffs' within-class selection gains one input (the pinned row's `cohort`) — a bounded contract change while the class model and default-move renewal stay intact.
* Bad, because the key widens to eight columns and every key-enumerating surface must be touched now — accepted as the cost of not migrating the key post-launch.

## More Information

Industry survey and the default-move vs default-stay analysis: review decision D-02
([`../DECISIONS.md`](../DECISIONS.md)). Interacts with D-04 (per-generation window bound at
`grandfatherUntil`) and D-05 (retirement vs pending cutover) — both apply per generation.

## Traceability

- **PRD**: §2.2 (canonical scope key), §1.4 Glossary (`cohort`, Grandfathering, `priceEligibility`), §6.8 (`fr-supersession`), §6.5 (`fr-grandfathering-eligibility`), §17.4 (duplicate-scope rule), §17.5 (cutover mechanism), §9.2 (Tariffs contract)
- **Design**: [`../design/01-foundation.md`](../design/01-foundation.md) §4.1 (scope key), §3.7 (`pricing_price` columns); [`../design/07-pricewindow-linkage.md`](../design/07-pricewindow-linkage.md) (cutover, eligibility resolution)
- **Supersedes in part**: ADR-0001's seven-column statements (the additive-extension decision itself is unchanged)
