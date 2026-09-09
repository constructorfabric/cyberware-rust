---
status: accepted
date: 2026-07-10
decision-makers: "BSS Product Catalog team"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0003: PriceWindow Machinery Consolidated into the Pricing Gear

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Atomic batch window operation in the UC contract](#atomic-batch-window-operation-in-the-uc-contract)
  - [Saga with compensation over the external UC](#saga-with-compensation-over-the-external-uc)
  - [Consolidate window ownership into the pricing gear (chosen)](#consolidate-window-ownership-into-the-pricing-gear-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-pricing-adr-pricewindow-consolidation`

## Context and Problem Statement

The design inherited `PriceWindow` scheduling, UTC activation jobs, and `PriceWindow*` event
mechanics as an **external boundary**: the "effective-dating price-windows use case"
(`UC-effective-dating-price-windows-202601121200`, from the January-2026 monolithic
*Catalog Service* PRD). Slice 7 held only linkage refs plus an event-driven **mirror** of
window state, with fail-closed degradation on mirror lag.

The review (finding D-03) showed this boundary unimplementable and ownerless:

- The grandfathering cutover requires **atomic multi-window changes** (shorten the
  predecessor's `effectiveTo` + schedule the generation copy + schedule the successor as one
  unit). The design claimed "one transaction" — impossible across a component boundary with
  local ACID; a partial failure (shorten committed, schedules lost) strands every subscriber
  of the plan uncovered at the cutover instant.
- The UC's runtime owner never existed: it targets the defunct monolithic "Catalog Service";
  its sibling UCs became the **registry** (product-sku-management) and **this PRD**
  (plan-price-modeling); nothing else consumes it (the registry is unmentioned in it).
- Its two normative deltas (4-axis key; gap-fallback-to-base-price) had already been
  superseded by this PRD, and the atomicity requirement had already been assigned to it via
  the consolidate-UC open question (PRD §15, owner: Architecture).
- The confirmed topology: all pricing slices are **one deployable gear over one PostgreSQL**;
  Tariffs is a separate gear. Windows are *price* windows — attached to price rows, validated
  at price publish, consumed via this gear's read model.

Who owns the `PriceWindow` store, state machine, activation job, and event production — and
how does the cutover get its atomicity?

## Decision Drivers

* Cutover atomicity is a coverage-safety invariant: no instant may be left uncovered by a partially-applied multi-window unit.
* The failure mode of any cross-component protocol (partial state latent until the cutover instant) is exactly the class the fail-closed doctrine exists to eliminate.
* A separate component for price-window mechanics is artificial distribution: no second consumer exists, and the mirror + lag-alarm machinery existed only to compensate for the boundary.
* Downstream consumers (Rating, Tariffs) depend on the frozen manifest `PriceWindow*` event names — the producer may move, the contract may not.
* Retirement (D-05), multi-key cutovers (D-28), and coverage checks all simplify to single-transaction logic if windows are local.

## Considered Options

* Keep the UC external; add an **atomic batch window operation** to its contract.
* Keep the UC external; run the cutover as a **saga** with compensation.
* **Consolidate window ownership into the pricing gear** (chosen).

## Decision Outcome

Chosen option: **consolidate**. Slice 7 owns, inside the pricing gear:

* the **`pricing_price_window`** store (tenant-scoped; half-open `[from, to)` intervals; non-overlap per canonical scope key enforced in every mutation) and the window **state machine** `scheduled → active → expired` / `scheduled → cancelled`, with historical immutability under the same `REVOKE` + column-whitelist trigger discipline as `pricing_price`;
* the **scheduling/cancellation/adjustment API** (`plan × write`; every mutation runs coverage/gap validation in-transaction — no side door exists);
* the **UTC activation/expiration job** as a coordination-lease singleton (idempotent; events ordered per `(tenant, plan)`);
* **`PriceWindow*` event production** from the gear outbox — the frozen manifest names (`PriceWindowScheduled`/`Activated`/`Expired`/`Cancelled`) are preserved; only the producer changes.

The cutover's multi-window unit (and retirement's unwind of a live cutover, D-05) is thereby
a **local ACID transaction**. The mirror (`pricing_price_window_link`), the `mirror_lag` and
`coverage_gap` alarms, and review item D-26 (routing of "UC-side" mutations) are **deleted /
dissolved** — no external mutation path exists. The legacy UC document is retained as
**scenario source material only** (banner-marked), with explicit dispositions: FX rate-lock
**rejected** (the catalog performs no FX — Tariffs/PLAL), subscription/revenue impact preview
**out of catalog scope** (needs Subscriptions data), the `suspended` window state **not
adopted**. The PRD §15 consolidation question is **answered** (formal Architecture ack
pending).

### Consequences

* Cutover atomicity becomes a **property of the transaction**, not a protocol to design — D-03 dissolves; multi-key cutovers (D-28) and retirement unwind (D-05) are the same one-transaction shape.
* Coverage/gap checks read owned tables directly; the fail-closed-on-stale-mirror degradation mode disappears (there is no mirror to go stale).
* The gear gains a scheduled background job (activation/expiration) — the coordination-lease library was already an internal dependency for read-model warm re-drive.
* `PriceWindow*` consumers (Rating caches, Tariffs) are unaffected: names, ordering, and delivery semantics unchanged; the manifest §4.1 producer note flips from "consumed" to "produced" here.
* The scheduler/timeline **UI** concerns from the legacy UC land in the Frontend DESIGN, not here.
* If a future artifact class ever needs generic effective-dating, it builds its own — *price* windows are not a shared platform service (nothing consumed them but pricing).

### Confirmation

* Design review: no mirror/`window_ref`/UC-contract references remain in the design set; S7 owns the table, state machine, job, and events; the S5 endpoint map covers the window API.
* Integration test: a simulated failure on the successor-schedule step of a cutover rolls back the shorten and both schedules — no partial window state at any instant; lease takeover activates a due window exactly once.
* Cross-team: Rating confirms `PriceWindow*` consumption is unchanged; Architecture acks the §15 answer (no standalone effective-dating PRD).

## Pros and Cons of the Options

### Atomic batch window operation in the UC contract

Ask the UC's (hypothetical) owner to expose shorten+schedule+schedule as one atomic call.

* Good, because it keeps the manifest's original component split and gives true atomicity.
* Bad, because the UC has **no runtime owner to ask** — the request would first have to invent the component, for a single consumer.
* Bad, because the mirror machinery (lag alarm, fail-closed degradation, D-26 routing question) survives.

### Saga with compensation over the external UC

Order the operations (schedules first is impossible — the successor overlaps the still-long
predecessor; so shorten first), compensate on failure before the cutover instant.

* Good, because it needs no new UC capability beyond cancel + **extend** (un-shorten).
* Bad, because "extend back" is itself a new contract capability, and a failed compensation leaves the latent gap guarded only by an alarm and an operator's reaction time until T.
* Bad, because every future multi-window operation (multi-key cutover, retirement unwind) re-inherits the saga complexity.

### Consolidate window ownership into the pricing gear (chosen)

* Good, because atomicity, coverage safety, and the D-05/D-26/D-28 simplifications fall out of one move; the artificial distribution and its compensating machinery disappear.
* Good, because it matches the confirmed topology (one gear, one database) and the factual consumer set (only pricing).
* Neutral, because the gear takes on the activation job and the window API surface — bounded, and the lease/authz/audit infrastructure already existed.
* Bad, because the manifest's original component sketch is superseded — accepted; the §15 question existed precisely to settle this, and the event contract (the real cross-team surface) is preserved.

## More Information

Full decision record: D-03 in [`../DECISIONS.md`](../DECISIONS.md) (incl. the source-UC
investigation); the legacy UC carries a consolidation banner
(`vhp-architecture/docs/bss/prd/PRD-product-catalog-marketplace-202601120119/UC-effective-dating-price-windows-202601121200.md`).

## Traceability

- **PRD**: §2.1 (boundary note), §9.2 (`cpt-cf-bss-pricing-contract-pricewindow` — internalized), §13 (dependency row), §15 (consolidation question — answered), §4 event alignment (producer note)
- **Design**: [`../design/07-pricewindow-linkage.md`](../design/07-pricewindow-linkage.md) (owner: store, state machine, API, job, events); [`../design/01-foundation.md`](../design/01-foundation.md) §1.2 (event set), §3.4/§3.5; [`../design/11-lifecycle.md`](../design/11-lifecycle.md) (retirement invokes the owned flow); [`../design/12-operator-efficiency.md`](../design/12-operator-efficiency.md) (bulk window ops are local writes)
- **Related ADRs**: `cpt-cf-bss-pricing-adr-canonical-scope-key` (the non-overlap key), `cpt-cf-bss-pricing-adr-grandfathering-cohort-axis` (per-generation windows)
