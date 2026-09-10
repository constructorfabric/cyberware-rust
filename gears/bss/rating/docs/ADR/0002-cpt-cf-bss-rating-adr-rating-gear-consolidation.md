---
status: accepted
date: 2026-07-11
decision-makers: "BSS Rating/Tariffs owner (single owner for both parts)"
---

Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

# ADR-0002: One `rating` Gear — Consolidate Tariffs (Evaluation Core) and the Rating Pipeline

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Target structure](#target-structure)
  - [What lands where (scope split)](#what-lands-where-scope-split)
  - [Naming table](#naming-table)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Migration Plan (impact map)](#migration-plan-impact-map)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Keep two gears, fix wording only](#keep-two-gears-fix-wording-only)
  - [Rename `tariffs` → `rating-core`, keep two gears](#rename-tariffs--rating-core-keep-two-gears)
  - [One `rating` gear (chosen)](#one-rating-gear-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-bss-rating-adr-rating-gear-consolidation`
*(this ADR's own ID migrates to the `rating` prefix in the mechanical rename commit, like every
other tariffs id).*

## Context and Problem Statement

The domain currently splits classic **rating** across two gears with misleading names:

- The gear named **Tariffs** owns no tariffs. Tariff-as-data (resource @ price, models, windows,
  price overlays) is the **Pricing** gear's SoR — the seam analysis (SEAMS, T-D-01…T-D-08) moved
  every definition-ownership claim there under *adopt, don't fork*. What the gear actually owns
  is the **pure price-decision function** (steps 1–9, determinism, snapshot composition) — in
  classical BSS taxonomy, *rating proper*.
- The gear named **Rating** is the operational half — usage ingestion, windowed `Q` aggregation
  (mediation), dedup, persistence, orchestration — and its PRD
  (`PRD-rating-engine-202604031200`, VHP-810) is **draft/empty**; its high-scope items ("tiered
  pricing evaluation", "deterministic outputs + pricingSnapshotRef") were explicitly migrated
  into the Tariffs PRD (§2.2).
- Physically there is already **one deployable**: PLAL is normatively a logical module inside the
  BSS Rating deployable (manifest §4.2; Tariffs PRD §2).

The 2026-07-11 tri-review exposed the split as load-bearing debt: money-affecting duties had to be
assigned to the neighbour gear as **cross-PRD obligations against an empty PRD** — the period tick
(T-D-15), delta-dedup ownership (T-D-11), `CommitmentBalanceEffect` publication (T-D-10) — and the
Rating handoff stayed "intent-stable but schema-unstable" because its ratifying counterpart does
not exist (review finding C6). Two gears, one deployable, one empty PRD, and a gear name that
contradicts its content: what should the domain structure and naming be?

## Decision Drivers

* A gear name must encode **responsibility** (what it owns), not the subject vocabulary; "tariff"
  correctly names Pricing's rate definitions, not this gear's evaluation function.
* The **pure-core boundary** (determinism, byte-identical replay, frozen inputs) is the design's
  crown jewel and must survive any restructuring — preferably with *stronger* enforcement than a
  documentation split.
* Cross-PRD obligations between the two halves are really **internal design decisions** of one
  domain; routing them through seam machinery adds cost and no protection.
* Cost window: the gear has **no code yet** (docs only) and Design lock has not happened — rename
  cost is at its lifetime minimum.
* Conway check passed: both halves have the **same owner** (confirmed 2026-07-11), so one gear
  does not create split ownership.

## Considered Options

1. Keep two gears as-is (`tariffs` + `rating`), fix only the PRD's "owns" wording.
2. Rename `tariffs` → `rating-core`, keep two gears (`rating-core` + `rating`).
3. **One `rating` gear**: evaluation core as an isolated no-I/O crate + pipeline slices absorbing
   the VHP-810 scope (chosen).

## Decision Outcome

Chosen option: **one `rating` gear**. The entire rating domain — the pure evaluation core
(the current Tariffs design set) and the operational pipeline (mediation, `Q`, dedup,
persistence, period tick) — lives in `gears/bss/rating/` as one gear with one PRD and one design
set. The pure-core boundary moves from a gear boundary to a **crate boundary**: `rating-core` is
a dependency-clean crate (no I/O deps — no tokio/sqlx/http; enforced by the Cargo dependency
graph plus a CI deny-list) exercised by golden fixtures in isolation. The word **"tariff"
returns to the Pricing vocabulary** (rate definitions / rate card) and stops naming this gear.

### Target structure

```text
gears/bss/rating/
├── docs/
│   ├── PRD.md                  ← current Tariffs PRD, retitled; §2.2 absorbs VHP-810 scope
│   ├── DESIGN.md               ← index regrouped: core slices + pipeline slices
│   ├── DECISIONS.md, SEAMS.md  ← retitled Rating ⇄ Pricing
│   ├── ADR/
│   └── design/
│       ├── 01–11 …             ← evaluation-core slices (this design set, ~as-is)
│       └── 12–16 …             ← pipeline slices (new; content = the duties today assigned
│                                  to the "Rating" actor + T-D-10/11/15)
├── rating-core/                ← (when code starts) pure crate: evaluate()/reresolve(),
│                                  steps 1–9, guards; zero I/O dependencies
└── …                           ← pipeline crates per ToolKit gear layout
```

Pipeline slice set (initial): **12** usage ingestion & normalization; **13** windowed `Q` store
(single-writer, per-slice attribution + `bandOffsetQ`) & usage/delta dedup (T-D-11);
**14** evaluation-unit synthesis & the period tick (T-D-15) & context assembly / read-model pin;
**15** rated-output persistence & the RatedCharge/BillableItem mapping (slice 11 §4.1 table) &
`CommitmentBalanceEffect` publication + cascade orchestration (T-D-10);
**16** Billing handoff & operations/scale.

### What lands where (scope split)

| Current content | Destination |
|---|---|
| PRD §6 semantics; slices 01–07 (pipeline order, selection, models, overlays, commitments, coupons, FX) | **rating-core** — unchanged in substance |
| Slice 08 replay/diff/reversal math; slice 09 split geometry, proration math, obligation shapes | **rating-core** (pure math); their *triggers* (correction ingestion, period tick) → pipeline |
| Slice 10 publish validators, rev-share pass-through, ASC refs | **rating-core** artifacts, registered in the pricing publish engine (unchanged) |
| Slice 11: five upstream contracts (Pricing, Subscriptions, Finance, Promotions, Billing-inbound) | **gear boundary contracts** of the rating gear (unchanged in substance) |
| Slice 11 §4.1 Rating handoff | becomes the **internal core↔pipeline crate API** (no longer a cross-gear contract) |
| Duties assigned to the "Rating" system actor (Q aggregation, dedup, unit synthesis, period tick, persistence, balance effects, cascade routing) | **pipeline slices 12–16** (graduate from actor description to first-class design) |
| Rating → Billing downstream handoff (previously the neighbour's undocumented duty) | **new gear boundary contract** (pipeline slice 16) |
| `PRD-rating-engine-202604031200` (VHP-810, draft/empty) | **absorbed**; upstream copy legacy (not maintained) |

### Naming table

| Old | New | Notes |
|---|---|---|
| gear `tariffs` (`gears/bss/tariffs/`) | gear `rating` (`gears/bss/rating/`) | `git mv`, history preserved |
| `cpt-cf-bss-tariffs-*` (315 unique ids / 488 occurrences) | `cpt-cf-bss-rating-*` | mechanical; includes this ADR's id |
| **PLAL** (implementation component) | **`rating-core`** (crate) | PLAL retired alongside the already-deprecated "Tariff Engine" |
| "Tariffs" as gear/domain name | "Rating" (gear); core vs pipeline as parts | |
| "tariff evaluation" (process) | "evaluation" / "price resolution" | |
| "tariff line", `TariffLineKey` | "charge line", `ChargeLineKey` | |
| "resolved tariff outcome", `ResolvedTariffOutcome` | "resolved price outcome", `ResolvedPriceOutcome` | |
| `…pricing-actor-tariffs` (pricing side, historical id) | `cpt-cf-bss-pricing-actor-rating` | **merged into the pre-existing rating actor** (commit C); plus prose mentions of "Tariffs" in pricing docs |
| word "tariff" | reserved for Pricing-gear rate definitions (rate card sense) only | |

### Consequences

* T-D-10 / T-D-11 / T-D-15 stop being cross-PRD obligations to an empty neighbour and become
  intra-gear design (pipeline slices), each still traceable to its decision row.
* Review finding C6 dissolves: the outcome-mapping table (slice 11 §4.1) becomes the actual
  pipeline design rather than a proposal awaiting a ratifier.
* The pure-core fence gets **stronger**: from a documentation boundary to a compiler-checked
  crate boundary + CI deny-list + isolated golden fixtures. Normative rule carried over: the core
  is invoked with frozen inputs only; no step evaluator reaches live state.
* Quote-time/estimate serving (open item) becomes cheap to extract later: `rating-core` is a
  standalone crate by construction.
* The evaluation design set (slices 01–11) is **not** re-authored — only re-homed and re-titled;
  its slice numbering and internal cross-references survive.
* One PRD grows: pipeline FR sections must be written (from T-D-10/11/15 + actor duties). This is
  work that VHP-810 owed the program anyway.

### Confirmation

* `cfs validate` (0 errors) and `cfs validate-toc` stay green after each migration commit.
* No orphan **ids**: `grep -r "cpt-cf-bss-tariffs-"` over the repo returns zero after Commit C (outside this ADR's historical naming table). This gate covers the id-prefix half of Commit C **only** — it passing does not certify Commit C done.
* No stale **prose**: the "Tariffs"-prose half of Commit C has its own check — `grep -rniw "tariffs" gears/bss/pricing/docs --include='*.md'` returns zero over the **living** pricing docs (PRD.md, DESIGN.md, design/): pricing's DECISIONS.md and ADR/ deliberately retain the historical naming as dated records (same convention as this gear's register) and are excluded. As of 2026-07-28 this check still fails (~300 prose hits) — Commit C's prose half is **open**, tracked in [`../DECISIONS.md`](../DECISIONS.md) migration residues.
* The dependency fence exists from the first code commit: `rating-core`'s `Cargo.toml` carries no
  I/O dependencies, and CI denies their introduction.
* Golden fixtures for the core run without the pipeline (fixture-only harness).

## Migration Plan (impact map)

Executed as ordered commits, each independently green:

- **Step 0 — baseline**: commit the pending 2026-07-11 review-closure batch (slices 02–11
  authoring + T-D-09…T-D-15 edits) so the rename is a clean, mechanical diff.
- **Commit A — mechanical rename** (this repo, rating side): `git mv gears/bss/tariffs
  gears/bss/rating`; rewrite ids (`cpt-cf-bss-tariffs-*` → `cpt-cf-bss-rating-*`),
  `CONFLUENCE_TITLE`s, and the naming-table terms across the moved docs; `cfs validate` +
  `validate-toc`.
- **Commit B — content**: PRD §1.1/§2.1 rewritten to "owns evaluation, not definitions"
  (closes the stale *owns commercial price rules…* wording); §2.2 records VHP-810 absorption;
  system actor "Rating & Charging" graduates from actor to pipeline slices; DESIGN.md index
  regrouped (core 01–11 / pipeline 12–16); pipeline slice skeletons 12–16 created; SEAMS.md
  retitled "Rating ⇄ Pricing"; DECISIONS.md wording (T-D-10 "cross-PRD: mirror in
  Contracts/Rating" → "Contracts"; T-D-11/15 now intra-gear).
- **Commit C — pricing side** (same owner confirmed): `…pricing-actor-tariffs` →
  `…-actor-rating`; "Tariffs" prose mentions across pricing PRD/DESIGN/DECISIONS/design/01–10/ADRs
  (≈279 occurrences incl. fixtures like `inst-rv-tier-q` wording) → "Rating (evaluation core)".
- **Upstream not maintained**: `gears-rust` is the canonical home; the BSS manifest §4.2 PLAL-placement
  caveat and `PRD-rating-engine-202604031200` (VHP-810) live only in upstream vhp-architecture, which
  is legacy provenance — no back-port or supersession-marking obligation.
- **Artifacts board**: keep `/tariffs` URL live (published overview), add `/rating` alias at next
  content update.
- **Rollback**: each commit is mechanical and independently revertible; `git mv` preserves
  history for blame/log continuity.

## Pros and Cons of the Options

### Keep two gears, fix wording only

* Good: zero migration cost.
* Bad: name keeps lying ("Tariffs" owns no tariffs); cross-PRD obligations against an empty PRD
  persist; "which rating?" ambiguity persists; the empty VHP-810 still owes a PRD nobody owns.

### Rename `tariffs` → `rating-core`, keep two gears

* Good: fixes the misnomer; keeps gear-level fence around the core.
* Bad: same migration cost as consolidation but keeps the artificial seam: handoff stays a
  cross-gear contract needing a ratifier, pipeline PRD still empty, two doc sets for one
  deployable and one owner. Justified only by split team ownership — which does not exist here.

### One `rating` gear (chosen)

* Good: names encode responsibility; obligations become internal design; the pipeline PRD gap is
  filled in the same doc set; the core fence strengthens (crate + CI instead of docs); one owner,
  one deployable, one gear — structure matches reality.
* Bad: one larger design set (mitigated by the existing slice structure); ~315 ids and the
  pricing-side prose must migrate (mechanical, one-time, at the cheapest possible moment);
  discipline required so the core/pipeline split inside one gear stays sharp (mitigated by the
  crate fence and fixture gate).

## More Information

Naming rationale: in classical BSS taxonomy the current "Tariffs" content *is* rating (applying
prices to usage), while the current "Rating" gear is mediation + orchestration; "tariff" denotes
the rate definitions owned by Pricing. See the ownership matrix in [`../SEAMS.md`](../SEAMS.md)
and the scope-migration record in [`../PRD.md`](../PRD.md) §2.2.

## Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §1.1 (purpose), §2 (PLAL deployment note this ADR supersedes),
  §2.1 (naming), §2.2 (scope migration, VHP-810).
- **Decisions**: T-D-16 (this consolidation), T-D-10/T-D-11/T-D-15 (obligations becoming
  intra-gear) — [`../DECISIONS.md`](../DECISIONS.md).
- **Seams**: [`../SEAMS.md`](../SEAMS.md) (retitled Rating ⇄ Pricing in Commit B).
- **Design**: [`../design/01-foundation.md`](../design/01-foundation.md) §3.8 (deployment),
  [`../design/11-consumer-contracts.md`](../design/11-consumer-contracts.md) §4.1 (handoff → internal API).
