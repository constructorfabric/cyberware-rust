Created:  2026-08-24 by Virtuozzo International GmbH
Updated:  2026-08-24 by Virtuozzo International GmbH

<!-- CONFLUENCE_TITLE: [BSS]: Subscriptions — Suspension, Renewal & Grace (Design) -->
<!-- Related: ../PRD.md, ../DESIGN.md, ../SEAMS.md | Upstream: Contracts (renewal/grace SoR), Payments (pre-check/retry-exhaustion), OSS (pause) | Downstream: Billing (dunning, collection artifacts), Notifications (notice delivery) | Owners: BSS Subscriptions team -->

# DESIGN — Suspension, Renewal & Grace (Slice 4)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-design-suspension-renewal-grace`

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
  - [4.1 Suspend / Resume and OSS Pause (normative)](#41-suspend--resume-and-oss-pause-normative)
  - [4.2 Billing-Only Pause Posture (normative)](#42-billing-only-pause-posture-normative)
  - [4.3 Renewal Evaluation, Auto vs Manual (normative)](#43-renewal-evaluation-auto-vs-manual-normative)
  - [4.4 Grace Ladder and Policy (normative)](#44-grace-ladder-and-policy-normative)
  - [4.5 Notices and Opt-Out (normative)](#45-notices-and-opt-out-normative)
  - [4.6 Dunning Handoff (normative)](#46-dunning-handoff-normative)
- [5. Traceability](#5-traceability)

<!-- /toc -->

## 1. Architecture Overview

### 1.1 Architectural Vision

This slice owns the **posture and time-driven** transitions: suspend/resume, the billing-only
`collectionPaused` posture, and the renewal → grace ladder. All of them route through the Foundation
commit path and are **governed, reversible, auditable** — never soft deletes or silent state drift
([`../PRD.md`](../PRD.md) §6.4, §6.5). The renewal job is Contract-driven: Subscriptions executes and
audits; the commercial terms (grace length, ladder, regional templates) are Contract SoR.

The load-bearing risk (**SUB-C1**) is that the upstream Contracts PRD does **not yet author** the
renewal/grace SoR; until it does, the **platform defaults govern** (7-day grace, 30/14/7/1 notices,
hybrid exit). The slice also owns **SUB-B2** (`collectionPaused` artifact treatment with Billing),
**SUB-F1** (Payments signals), **SUB-B5** (dunning handoff), and **SUB-E2** (OSS pause on suspend).

### 1.2 Architecture Drivers

#### Functional Drivers

| Requirement | Design Response |
|-------------|-----------------|
| `cpt-cf-bss-subscriptions-fr-suspend-resume` | `suspend`/`resume` through the Foundation gate; revoke/freeze then re-issue entitlements (slice 05); OSS deprovision/pause / reprovision confirmed by events (§4.1). |
| `cpt-cf-bss-subscriptions-fr-suspension-billing-posture` | Suspension-vs-billing (pause recurring vs continue for reserved capacity) is explicit product policy in subscription attributes + contract clauses (§4.1). |
| `cpt-cf-bss-subscriptions-fr-collection-pause` | `collectionPaused` is an auditable window (start/end/limit/reason) **posture on `active`** — service untouched, recurring emission suppressed/deferred per policy (§4.2; SUB-D-03). |
| `cpt-cf-bss-subscriptions-fr-renewal-evaluation` / `cpt-cf-bss-subscriptions-fr-renewal-auto-manual` | A renewal job evaluates term/`endDate`, extends on success, triggers the failed path on failure; auto requires a valid payment method + contract allow; manual is an explicit `TransitionRequest`; attempts keyed against double extension (§4.3). |
| `cpt-cf-bss-subscriptions-fr-failed-renewal-ladder` / `cpt-cf-bss-subscriptions-fr-grace-policy` | A testable grace ladder: 7-day default, paused next-term recurring, evaluated fields stored for replay, hybrid exit (elapsed OR retry-exhausted) (§4.4). |
| `cpt-cf-bss-subscriptions-fr-renewal-notices` | Notice triggers at 30/14/7/1 days (Contract/template override within Legal bounds); opt-out = scheduled non-renewal at term end; delivery = Notifications (§4.5). |

#### NFR Allocation

| NFR theme | Allocated To | Design Response | Verification / Status |
|-----------|--------------|-----------------|-----------------------|
| `cpt-cf-bss-subscriptions-nfr-lifecycle-latency` | Suspend/resume commit path | Synchronous commit class (p95 < 1s) = the **intent commit** — suspend/resume carry OSS legs, so their status commits are async and outside the bound (slice 01 NFR row is the authority) | Load test; baseline (workshop-pending) |
| `cpt-cf-bss-subscriptions-nfr-recurring-cut` | Grace recurring suppression | Blocked next-term recurring emits only on renewal success / §4.3b revival; on grace failure it is never emitted (§4.4) | Reconciliation §17.1 |

#### Key ADRs

No slice-local ADR; the renewal/grace SoR split is governed by SEAMS **SUB-C1** (Contracts SoR;
platform default until authored) and the pause posture by SUB-D-03.

### 1.3 Architecture Layers

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-tech-stack-rnw`

| Layer | Responsibility | Technology |
|-------|----------------|------------|
| Application | Suspend/resume + pause posture handlers; the renewal job; notice + grace ladder | Rust module in the `subscriptions` gear |
| Domain | `collectionPaused` window, renewal-evaluation record (evaluated fields), grace-ladder state, notice schedule | Rust; GTS + Rust domain structs |
| Infrastructure | Grace-evaluation table; renewal job coordinated via the lease library | PostgreSQL, SecureORM |

## 2. Principles and Constraints

### 2.1 Design Principles

#### Governed, reversible posture

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-governed-posture-rnw`

Suspension and pause are governed, reversible posture changes — not soft deletes; every transition
is Policy-gated, evented, and audited ([`../PRD.md`](../PRD.md) §6.4).

#### Contract is the terms SoR; Subscriptions executes

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-principle-contract-sor-rnw`

Renewal terms, grace length, ladder, and regional templates are Contract SoR; Subscriptions runs the
job, stores **evaluated fields** at evaluation time, and audits — it invents no commercial term
([`../PRD.md`](../PRD.md) §6.5; SEAMS **SUB-C1**).

### 2.2 Constraints

#### Grace defaults are platform-testable

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-grace-defaults-rnw`

Until Contracts authors the SoR, the **7-day grace / 30-14-7-1 notices / hybrid exit** platform
defaults govern and MUST be product-testable; Contract/template override only within Legal bounds
([`../PRD.md`](../PRD.md) §6.5; SEAMS **SUB-C1**).

#### Idempotent renewal attempts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-constraint-renewal-idempotent-rnw`

Renewal attempts are keyed to prevent **double term extension**; a retry-driven job never extends
twice ([`../PRD.md`](../PRD.md) §6.5).

## 3. Technical Architecture

### 3.1 Domain Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-domain-model-rnw`

- **`CollectionPauseWindow`** — `start`, `end`/`limit`, `reason`; posture on `active`, collection-scoped only.
- **`RenewalEvaluation`** — the evaluated fields at term-end (grace length, ladder variant, billing posture, `graceEndsAt`) frozen for replay + idempotent jobs.
- **`GraceLadderState`** — grace start, elapsed/retry status, exit trigger.
- **`NonpaymentDwell`** — the SUB-D-16 evaluated deadline for the current nonpayment-suspension episode: `suspendedAt` (anchor), `resolvedDwell` + its source (`ladder | tenant | platform`), `deadlineAt`, appended **hold records** (dispute start/end — SUB-D-22), and the void marker (payment resolution / exit from `suspended` / fired). Episode-scoped: a later suspension gets a fresh record (2026-08-01 wave-3 review #24e — the field set backing §4.4's dwell rules).
- **`NoticeSchedule`** — the 30/14/7/1 trigger set + opt-out flag.

### 3.2 Component Model

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-component-renewal-rnw`

- **`SuspendResumeHandler`** — the posture transitions through the Foundation gate; coordinates OSS pause/reprovision + entitlement freeze/re-issue (slice 05).
- **`CollectionPauseHandler`** — sets/clears the `collectionPaused` window; signals Billing the artifact treatment.
- **`RenewalJob`** — the coordinated singleton evaluating term/`endDate`, extending on success, entering the failed path on failure.
- **`GraceLadder`** — drives the 7-day (or Contract) window, the paused next-term recurring, and the hybrid exit.
- **`NoticeScheduler`** — emits notice triggers + processes opt-out as scheduled non-renewal.

### 3.3 API Contracts

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-interface-renewal-rnw`

`suspend`/`resume`/`renew` operations + the `collectionPaused` set/clear operation; renewal-monitoring
read models (failing renewals, `graceEndsAt`, ladder variant) power the Finance UC
([`../PRD.md`](../PRD.md) §10 renewal-monitoring). Payments failure-signal + Billing dunning wire
contracts are owned by [`09-consumer-contracts.md`](./09-consumer-contracts.md).

### 3.4 Internal Dependencies

Depends on [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (commit path, gate,
`IntentScheduler` for scheduled non-renewal) and [`05-entitlements.md`](./05-entitlements.md)
(freeze/re-issue on suspend/resume). Feeds [`08-events-billing.md`](./08-events-billing.md)
(suspend/resume/collection events).

### 3.5 External Dependencies

| Dependency | What crosses the boundary | Contract |
|------------|---------------------------|----------|
| Contracts | Renewal terms, grace ladder, regional templates, `PriceOverride` | SEAMS **SUB-C1**, **SUB-C5** |
| Payments | Pre-check outcomes + retry-exhaustion declarations | SEAMS **SUB-F1** |
| Billing | Dunning execution; `collectionPaused` artifact treatment | SEAMS **SUB-B2**, **SUB-B5** |
| OSS | Deprovision/pause on suspend; reprovision on resume | SEAMS **SUB-E2** |
| Notifications | Notice + win-back delivery (triggers owned here) | SEAMS **SUB-F2** |

### 3.6 Interactions and Sequences

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-flow-renewal-grace-rnw`

**Renewal + grace** (refines `cpt-cf-bss-subscriptions-seq-renewal-grace`): `RenewalJob` at term end
→ **(0) pending-intent branch** (2026-07-28 review fix — the executing sequence now carries the
branches its rules already mandated elsewhere): a pending non-renewal `ScheduledIntent` for this term
⇒ **no extension** — the intent fires instead (§4.3 ordering rule; slice 01 §4.3 suppression) →
**(0a) `autoRenew = false` and no pending manual `renew`** ⇒ no extension — the term-expiry path runs
(§4.3a: end-of-term `cancel`, reason `term_expired`) → **(0b) open `collectionPaused` window** ⇒
term extension proceeds but the payment pre-check / grace entry / dunning handoff are **deferred**
per SUB-D-12 (§4.2) → otherwise payment pre-check → **success**: extend term (keyed against double
extension) + eligibility-first snapshot re-resolution (§4.3, SUB-D-14 as amended); **failure**: `GraceLadder` starts (7-day default), the blocked
next-term recurring is **paused** (not emitted), notices fire, hybrid exit (interval elapsed OR
Payments retry-exhausted) → `suspended`/`cancelled` per Contract ladder; all transitions run through
the Foundation gate. `RenewalEvaluation` stores evaluated fields for replay. **Dwell branch
(SUB-D-16/22, 2026-08-01 wave-3 #24e):** on the exit to `suspended`, resolve + persist the
`NonpaymentDwell` deadline (§3.1); while the subscription remains `suspended` past `deadlineAt`
(holds excluded), the daily sweep **re-derives** the terminal `cancel` (reason =
`nonpayment_exhausted`) idempotently until it commits (§4.4); payment resolution, any committed exit
from `suspended`, or the firing itself voids the record.

### 3.7 Database Schemas and Tables

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-storage-renewal-rnw`

Owned here: `renewal_evaluation` (evaluated fields, `graceEndsAt`), `grace_ladder_state`, and the
`collection_pause_window` rows on the aggregate. Scheduled non-renewal rides the Foundation
`scheduled_intent`. Concrete DDL is Design.

### 3.8 Deployment Topology

- [ ] `p3` - **ID**: `cpt-cf-bss-subscriptions-deployment-rnw`

The `RenewalJob` + `NoticeScheduler` run as coordinated singletons **per tenant partition** via the
lease library — one lease per `orderingTenantId` shard, shard-parallel across partitions, so the
100K+/tenant scale target is not funnelled through one global instance (2026-07-15 review fix);
within a partition the per-aggregate ordering holds. **Intra-tenant parallelism (2026-07-15 review
fix):** because a single large tenant is one `orderingTenantId` shard, the daily-cut-class work
inside it is further sub-sharded by a stable hash of `subscriptionId` into N worker leases, so a
100K+/tenant renewal/notice sweep is not serialised through one worker; per-aggregate ordering is
preserved because a given `subscriptionId` always maps to the same sub-shard. N is a deploy-time
capacity knob, not a commercial one. Suspend/resume are control-plane transitions;
edges with OSS legs follow the Foundation async note ([`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md)
§3.6/§3.8).

## 4. Additional Context

### 4.1 Suspend / Resume and OSS Pause (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-suspend-resume-rnw`

- **Suspend** → `suspended`; revoke/freeze entitlements per policy; OSS deprovision/pause confirmed by events. **Resume** → `active`; **Policy allow mandatory**; re-issue entitlements; OSS reprovision ([`../PRD.md`](../PRD.md) §6.4; SEAMS **SUB-E2**). **Resume after a grace-driven suspension additionally requires the blocking payment failure resolved** (successful renewal/payment or an audited operator override) — `resume` alone never restores unpaid service ([`../PRD.md`](../PRD.md) §6.5; 2026-07-15 review fix). Resume also re-runs the overlap check (slice 03 §4.4 — entry into `active`). **Collision on resume (explicit, 2026-07-28):** an overlapping subscription acquired while this one was suspended fails the resume **closed** (`guard_violation`, the conflicting subscription(s) enumerated in the problem response) — no auto-rebind and no silent supersession of either side; remediation is operator-driven (cancel/re-scope the conflicting acquisition, or cancel+new this subscription) and the resume retries cleanly afterwards, so a paid-but-suspended subscription is never *silently* unresumable — every refusal names its blocker. The §4.3b ordering (payment → backdated term → resume) is unchanged.
- Suspension-vs-billing (pause recurring vs continue for reserved capacity) is **explicit product policy** in subscription attributes + contract clauses — never a silent assumption ([`../PRD.md`](../PRD.md) §6.4). **The policy and the interval now travel to the gear that prices the period (2026-07-28 billing-pass review #3, SUB-D-07 amendment):** the money-free period fact carries the period's **suspended interval(s)** (`[suspendedAt, resumedAt)` per episode, clipped to the period) and the **suspension-billing posture** (`pause_recurring | continue`), so rating can compute a suspension-prorated recurring line — including the mid-period case — while this gear keeps computing no money (WHEN/MATH intact; slice 08 §4.3 carries the field rule).

### 4.2 Billing-Only Pause Posture (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-collection-pause-rnw`

- `collectionPaused` is a **posture on `active`**: service + entitlements untouched; the recurring period fact for the paused window is emitted **marked `collectionPaused`** and Billing suppresses/defers the posting **per policy** — Billing owns the artifact treatment, AC 24 "not posted" holds ([`../PRD.md`](../PRD.md) §6.4; SUB-D-03, SEAMS **SUB-B2**).
- **Renewal during the window (SUB-D-12, AC 29):** renewal **evaluation and term extension continue** (the term stays deterministic for rating/Billing), but the payment pre-check, grace entry, and dunning handoff are **suspended** for renewals whose collection falls inside the window; the deferred collection runs when the window ends. A pause never converts into a payment-failure suspension by itself.
- **Pause applied while already in grace (2026-07-15 review fix):** a `pauseCollection` set on a subscription **already inside the grace ladder** (the common dispute/hardship reaction to a failed charge) **freezes the ladder** — the grace clock stops, dunning is held, and no exit to `suspended`/`cancelled` fires — for the duration of the window; on `resumeCollection` (or window end) the ladder resumes with the **remaining** grace time (elapsed time before the pause counts, time inside it does not). This is the same "collection deferred, service preserved" invariant as SUB-D-12 applied to an in-flight ladder rather than a fresh renewal; the pause-day limit (§15, Product/Billing) bounds indefinite deferral so a pause cannot be used to escape suspension forever. `GraceLadderState` records the freeze/thaw as evaluated fields for replay.
- The posture is an auditable window (start/end/limit/reason) bounded by Contract/Policy, set/cleared by the `pauseCollection`/`resumeCollection` transitions (SUB-D-08). **Cumulative bound (2026-08-01, wave-3 hardening):** the pause-day limit is **cumulative per term, not per window** — consecutive windows count together against a **90-day platform default** (tenant-configurable; Contract overrides; the Product knob refines, §15), evaluated at `pauseCollection` submit fail-closed — otherwise chained windows escape both the per-window limit and the §4.2 anti-escape rule ("a pause cannot be used to escape suspension forever" relied on a bound that did not exist). **Open (§15):** the knob's final value + resume proration — Product/Billing.
- **§4.2a — window-end executor (2026-07-28 review fix).** "(or window end)" now has a named mechanism: a `pauseCollection` carrying an **end date** persists, in the same commit, a **derived `ScheduledIntent` of kind `resumeCollection`** at that instant — so the existing `IntentScheduler` (slice 01 §3.6/§4.3, including the firing-failure taxonomy) is the executor. The firing commits the window-clearing transition through the Foundation path, emits `SubscriptionCollectionResumed`, thaws a frozen grace ladder with its remaining time, and triggers the **deferred collection** (the paused-window facts hand to Billing for posting). An **open-ended** window has no derived intent and clears only by an explicit `resumeCollection`. **Suspension while a window is open (2026-07-28 billing-pass review #6; class named 2026-08-01, SUB-D-23):** a `suspend` is NOT forbidden — the window **survives the suspension**, and the derived window-end firing hitting a `suspended` aggregate is classified **parked (state-precondition)** per the slice 01 §4.3 taxonomy's third class (the same rule as `convertTrial`-while-suspended in the slice 01 §4.2 matrix): the intent parks with its firing deadline **suspended** while the blocking state persists — a suspension outlasting the retryable grace horizon can no longer exhaust it to terminal (the wave-3 review #5 hole) — and the `resume` commit **re-arms** it (fires and closes the window with the full §4.2a effects); a terminal state voids it via the sweep. Without the class the firing would exhaust to terminal per the two-class taxonomy, the window would never close, and AC 29's deferred collection would silently die. **Terminal state first:** the slice-01 terminal sweep voids the derived intent and the terminal commit **closes the window** (audited); the deferred collection for facts already emitted still runs — cancellation ends service, it does not forgive the paused-window debt (Billing posts per its artifact treatment, SUB-D-03).

### 4.3 Renewal Evaluation, Auto vs Manual (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-renewal-rnw`

- The renewal job evaluates `Renewal` (`autoRenew`, term windows) from Contract; **auto** extends when the payment method is valid + contract allows; **manual** requires an explicit `TransitionRequest` of type **`renew`** (SUB-D-08) with the same idempotency rules ([`../PRD.md`](../PRD.md) §6.5).
- **Renewal vs a pending non-renewal intent (2026-07-19 review fix — F-04-3).** A scheduled non-renewal (opt-out, §4.5) and the `RenewalJob` both target term end. Ordering is defined, not raced: the `RenewalJob` **reads the pending-intent set first** and, if a non-renewal `ScheduledIntent` is pending for this term, does **not** extend — the non-renewal fires instead (the intent's suppression already blocks the extension per slice 01 §4.3). If the non-renewal firing **fails**, the slice-01 firing-failure taxonomy governs the outcome, **not** an implicit renewal: a *retryable* failure retries the non-renewal with its suppression held (the `RenewalJob` still does not extend while the intent is live); a *terminal* failure parks the intent `failed` with an operator alarm and the term is **held un-renewed** pending manual resolution — the subscription is never silently renewed because the customer's opt-out could not commit. Re-arming renewal after a terminal opt-out failure is an explicit operator/`renew` action, audited.
- Attempts are **keyed to prevent double term extension**: the term-extension effect is idempotent on `(subscriptionId, currentTermSequence)` — the monotonic index of the term being renewed — derived, not client-supplied, so a crashed-and-retried `RenewalJob` firing (or a duplicate manual `renew`) resolves to the **already-extended** term instead of extending twice (the same derive-the-key discipline as the scheduled-intent firing, slice 01 §3.6). **Every** successful renewal — auto and manual alike — re-resolves the pricing-side snapshot refs, **eligibility-first** (SUB-D-14 as amended 2026-07-28 — the billing-pass review caught the original "unconditional refresh" inverting pricing's mechanism): the re-resolution runs through pricing's eligibility machinery, so a **non-grandfathered** subscription re-binds to the current eligible row (published supersessions reach auto-renewing subscriptions — the decision's original goal), while a **grandfathered** subscription **keeps its pinned generation**: the refresh carries `priceEligibility` + `cohort` **forward** into the new ref (the pinned price id is the cohort's only carrier — pricing design/07, "no separate binding store" — so it is never blindly replaced), and the re-bind away from the generation fires **only at the first renewal after the bound generation's `grandfatherUntil` has passed**, signalled by pricing's `EligibilityExpirySignal` (SUB-P6). This is exactly the contract pricing sized its windows for (`grandfatherUntil` + one full cycle margin, *because* re-bind happens at the next renewal after expiry). The `(currency, region)` segment persists (slice 02 §4.2). **Scope boundary (SUB-D-26, 2026-08-01):** this carry-forward protects renewals of the *same* subscription only — a cancel+new execution (cross-currency/region/frequency, slice 03 §4.3) mints a fresh purchase gate and the cohort does **not** carry across the `supersedesSubscriptionId` pair; the loss is disclosed on the change preview before execution.
- **Late success inside grace:** the new term starts at the **old term end** (backdated — continuous coverage, no gap); the previously blocked next-term recurring fact is emitted with its **original** `(subscriptionId, billing period, lineKey)` key ([`../PRD.md`](../PRD.md) §6.5 grace policy 5).
- **§4.3a — `autoRenew = false` term expiry (2026-07-28 review fix, SUB-D-13).** A subscription created with `autoRenew = false` and holding **no** pending intent and no manual `renew` at `endDate` does not linger `active` unbilled: the `RenewalJob`, finding no extension path in the §3.6 sequence, fires an **end-of-term `cancel`** (reason = `term_expired`) through the Foundation commit path — the same mechanics as a scheduled opt-out, just system-derived instead of customer-scheduled; the firing-failure taxonomy applies to it identically (a transient blip never leaves the term ambiguously extended). Inside an open `collectionPaused` window the rule is unchanged — the pause defers **collection**, not lifecycle: with nothing to extend, the term still ends at `endDate` (window disposition at terminal: §4.2a). The PRD §6.5 carries the same rule (propagated with this fix).
- **§4.3b — payment resolution after a grace-driven suspension (2026-07-28 review fix, SUB-D-13).** Backdated term continuity extends past the grace exit: when the customer pays while `suspended` for nonpayment (the §4.1 resume precondition), the payment-resolution flow **re-runs the never-succeeded renewal** — the same idempotent term extension keyed `(subscriptionId, currentTermSequence)`, executed by the `RenewalJob` machinery on the payment-resolved signal — and the new term **backdates to the old term end**, exactly like late-success-inside-grace, so the anchor-derived period sequence of slice 08 §4.3 is never broken by a resume-time term start. The suspended gap therefore falls **inside** the new term; whether it is billed follows the explicit suspension-billing product policy (§4.1 pause-recurring vs continue), never an implicit charge. Ordering: payment resolution → backdated term extension → `resume` (§4.1) — a resume never precedes the term that covers it.
- **§4.3b as-of discipline (SUB-D-20, 2026-08-01 wave-3 review #1/#11/#13).** With the SUB-D-16 dwell the revival can run up to 90 days after the period it describes; every axis therefore resolves **as of the instant it describes, never as of the commit**: (a) the once-blocked period fact(s) are cut against the **effective-dated composition over the backdated period** — one fact per component interval overlapping it (SUB-D-21) — so an add-on that ended inside the blocked period is billed for its in-period stretch and one that ended before it is not, no matter when the revival commits; (b) the facts' **`payerTenantId` is the payer in force at the period start** (derived from committed transfer history) — a transfer executed during the suspension governs future periods only, matching the slice 07 in-flight default; (c) the revival renewal's **eligibility-first snapshot re-resolution (SUB-D-14) is evaluated as of the backdated term start** — pricing's published windows are effective-dated and answer as-of reads — so a grandfathered generation whose `grandfatherUntil` passed *during* the suspension still covers the backdated term it covered at its start, and the re-bind fires at the next real-time renewal; this is what keeps the revival inside pricing's `grandfatherUntil + one cycle` window margin by construction (the as-of instant is the old term end, never the commit instant; the ≤ 90-day-in-the-past read obligation is recorded on SEAMS SUB-P6). **Suspension without a pending renewal (wave-3 #11):** the term-extension step applies **only when the blocked operation was a term extension**. A nonpayment suspension caused by a **failed trial-conversion charge** (slice 06 §4.4) has no never-succeeded renewal — the paid phase and its term are already in force (conversion advances the boundary and is never rolled back) — so its revival degenerates to payment resolution → `resume`, and the blocked one-time conversion billable (SUB-D-24) emits on the payment-resolved signal; if the term expired *during* the suspension, the renewal that then failed-by-absence is a normal never-succeeded renewal and the full §4.3b ordering applies to it.

### 4.4 Grace Ladder and Policy (normative)

- [ ] `p1` - **ID**: `cpt-cf-bss-subscriptions-normative-grace-rnw`

- Default grace: **7 calendar days** from grace start (first auditable failed pre-check or aligned post-renewal billing failure); jurisdiction bounds come from Contract/regional template within Legal ([`../PRD.md`](../PRD.md) §6.5).
- Recurring during grace: the **blocked next-term recurring MUST NOT be emitted** while grace runs; on **success** it is emitted with its original key (§4.3); on **grace resolving to failure it is never emitted** (2026-07-28 billing-pass review #4 — the earlier "until … grace resolves to failure" literally licensed emitting a fact for a term that never started). If the §4.3b post-suspension path later **revives** that exact term (payment while `suspended` → backdated extension), the once-blocked fact **is then emitted with its original `(subscriptionId, billing period, lineKey)` key** — the same rule as late-success-inside-grace, and the key's uniqueness is what makes the whole lifecycle single-emission (never a silent no-op, never a double). **Precedence vs `collectionPaused` (explicit, 2026-07-28):** grace suppression governs **emission**, the pause governs **collection** — a pause window (open, frozen ladder per §4.2, or thawing at window end per §4.2a) never causes a grace-blocked fact to emit; §4.2a's deferred collection covers *already-emitted* facts only, and the blocked next-term fact emits solely on renewal success / §4.3b revival, pause or no pause. Usage-rated charges MAY continue until `suspended` unless Contract/Policy freezes them.
- Exit is **hybrid — whichever first**: grace interval elapses, OR Payments declares no further automated retries. Move to `cancelled` per contract-defined steps after suspend/final dunning ([`../PRD.md`](../PRD.md) §6.5) — **with a platform-default terminal step while Contracts is unauthored (SUB-D-16, 2026-07-28 billing-pass review #11)**: a subscription suspended for nonpayment dwells at most **90 days** (tenant-configurable; a Contract ladder overrides) — the effective dwell is **resolved at suspension time** (ladder → tenant → platform) and stored as an **evaluated deadline** (the same evaluated-fields replay discipline as `RenewalEvaluation`; later configuration changes govern future suspensions only, an in-flight deadline never moves) — after which the system fires an end-of-term-style `cancel` (reason = `nonpayment_exhausted`) through the commit path — the same immortal-state defect SUB-D-13 killed for `active` was still standing on the `suspended` side (expired term, no renewal, no facts, `archive` unreachable, §4.3b revivable forever). The §4.3b revival window is therefore **bounded by the dwell**: payment inside it revives per §4.3b; after the terminal step, winning the customer back is a new subscription. Subscription stores **evaluated fields** for audit + idempotent jobs + replay (SEAMS **SUB-C1**, **SUB-F1**).
- **Dwell deadline lifecycle (SUB-D-22, 2026-08-01 wave-3 review #3/#4).** The deadline is scoped to the **nonpayment-suspension episode** (`NonpaymentDwell`, §3.1) and its "never moves" rule is narrowed to configuration drift; the episode's own events govern it: **(a) voiding** — the record is voided by whichever comes first: **payment resolution** (the same signal that triggers §4.3b — so a customer who paid but whose `resume` fails closed on a §4.1 overlap collision, or who was resumed early by an audited operator override, can never be terminal-cancelled while paid: the void precedes the resume, not the other way round), any **committed exit from `suspended`**, or the terminal step's own firing. **(b) transfer** — a `transfer` from `suspended` (slice 01 §4.2 matrix: legal once the ladder is no longer running) **re-resolves** the deadline from the acquiring payer's ladder (ladder → tenant → platform), **anchored at the original suspension instant**, audited as a new evaluation on the same episode; if the re-resolved deadline is already past at transfer commit, the terminal step fires immediately after it — the approving transferee accepted an exhausted episode (slice 07). **(c) dispute hold** — a Payments-declared dispute/chargeback investigation **on the blocking charge** (SEAMS SUB-F1) appends a **hold record**: the dwell clock stops for the hold's duration and resumes on resolution; the hold has no service or collection effect (it is not a `pauseCollection`, which stays `active`-only) — it only stops the terminal clock while the debt itself is contested. **(d) a cancel that can commit** — the terminal `cancel` is **re-derived daily** while the subscription remains `suspended` past its (hold-adjusted) deadline, idempotent on the episode — a standing system obligation, not a one-shot intent, so a failed firing is re-attempted by construction; and its **deprovision leg is a no-op** where the suspend's recorded OSS work-order outcome already deprovisioned the resources (the firing consults the persisted outcome; only still-provisioned/paused resources get an OSS-blocking leg — slice 01 §3.6 async note), so the firing cannot exhaust `oss_unconfirmed` against resources that no longer exist and strand the state it exists to terminate.

### 4.5 Notices and Opt-Out (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-notices-rnw`

- Notice triggers at platform default **30/14/7/1 days** before term end; Contract/regional template MAY override within published bounds. **Triggers + intervals owned here; delivery = Notifications/Comms** ([`../PRD.md`](../PRD.md) §6.5; SEAMS **SUB-F2**).
- **Short terms:** intervals ≥ the term length are skipped — only offsets that fit inside the current term fire (a monthly term gets 14/7/1 by default, not a 30-day notice at term start); the effective set is an evaluated field for audit.
- **Price-change notice input (SUB-D-17, 2026-07-28 billing-pass review #9; inputs corrected 2026-08-01, wave-3 review #23):** the notice job additionally **derives a boolean per bound row** — "a scheduled supersession window, or the bound generation's `grandfatherUntil`, takes effect at/before the renewal instant" — by comparing the renewal instant against the pricing read model's **scheduled `PriceWindow` supersessions and the bound generation's `grandfatherUntil` date** (SUB-P6; slice 09 §4.2 lists the inputs). It is **not** derivable from `EligibilityExpirySignal`, which pricing computes at read time as `now ≥ grandfatherUntil` — true only at/after expiry, never 30 days ahead (the original text cited the signal and could not have armed any notice). When the boolean is true the **30-day notice is armed as a commercial notice** — the **`SubscriptionRenewalNoticeDue` payload carries the `priceChangePending` flag** (slice 08 §4.1 registry), Notifications renders the price-change variant. This gear still computes **no money** — it forwards the *fact* that a different row will be in force, not the amount; the amount, if the template wants one, is the Tariffs effective-price preview's job (pricing F-34). Without this input no notice could ever fire before the SUB-D-14 re-resolution, which happens **at** renewal — after the last time-based notice ([`../PRD.md`](../PRD.md) §16 non-compliant-auto-renewal risk).
- A renewal **opt-out** is a scheduled non-renewal at term end (cancel at term boundary; no further attempts), idempotent — via the Foundation `IntentScheduler`; opting back in is the `unschedule` of that intent (SUB-D-08). **`cancelMode = end_of_term` submitted during grace (2026-07-28 billing-pass review #21):** the term has already ended unextended, so "end of term" anchors to **`graceEndsAt`**; a late success inside grace (backdated extension) **re-anchors** the intent to the new term's end; grace resolving to failure makes the intent moot (the ladder's suspend/cancel exit governs, and a terminal exit sweeps it — slice 01 §4.3).

### 4.6 Dunning Handoff (normative)

- [ ] `p2` - **ID**: `cpt-cf-bss-subscriptions-normative-dunning-rnw`

- A post-renewal billing failure hands off to **dunning** (Billing/Payments §4.4–4.5); the same grace rules + triggers apply. Subscriptions emits the failure/grace signals + audit trail; **dunning execution + PSP webhook payloads are Billing/Payments + Design** ([`../PRD.md`](../PRD.md) §6.5; SEAMS **SUB-B5**, **SUB-F1**).

## 5. Traceability

- **PRD**: [`../PRD.md`](../PRD.md) §6.4 (`fr-suspend-resume`, `fr-suspension-billing-posture`, `fr-collection-pause`), §6.5 (`fr-renewal-evaluation`, `fr-renewal-auto-manual`, `fr-renewal-notices`, `fr-failed-renewal-ladder`, `fr-grace-policy`), §10 (renewal-monitoring UC), §7.1 (NFRs), §15 (pause mechanics open), §16 (Contracts-grace risk).

**Traces to**: `cpt-cf-bss-subscriptions-fr-suspend-resume`, `cpt-cf-bss-subscriptions-fr-suspension-billing-posture`, `cpt-cf-bss-subscriptions-fr-collection-pause`, `cpt-cf-bss-subscriptions-fr-renewal-evaluation`, `cpt-cf-bss-subscriptions-fr-renewal-auto-manual`, `cpt-cf-bss-subscriptions-fr-renewal-notices`, `cpt-cf-bss-subscriptions-fr-failed-renewal-ladder`, `cpt-cf-bss-subscriptions-fr-grace-policy` *(single-owner FR claims — the P2 traceability convention adopted 2026-08-01, wave-3 review #24h; shared mechanics other slices cite stay narrative, each FR has exactly one owning slice)*
- **Seams**: **SUB-C1**, **SUB-B2**, **SUB-F1**, **SUB-B5**, **SUB-E2**, **SUB-C5**, **SUB-F2** — [`../SEAMS.md`](../SEAMS.md).
- **Decisions**: SUB-D-03 (`collectionPaused`), SUB-D-12 (pause defers collection, not evaluation; §4.2/§4.2a), SUB-D-13 (term-boundary resolution; §4.3a/§4.3b), SUB-D-14 (eligibility-first re-resolution; §4.3), SUB-D-16 (nonpayment dwell; §4.4), SUB-D-17 (price-change notice input; §4.5), SUB-D-20 (as-of revival discipline; §4.3b), SUB-D-22 (dwell lifecycle; §3.1/§4.4), SUB-D-23 (parked firing class; §4.2a), SUB-D-26 (cohort × cancel+new boundary; §4.3) — [`../DECISIONS.md`](../DECISIONS.md).
- **Slices**: [`01-foundation-lifecycle.md`](./01-foundation-lifecycle.md) (gate, scheduler), [`05-entitlements.md`](./05-entitlements.md) (freeze/re-issue), [`08-events-billing.md`](./08-events-billing.md) (posture events), [`09-consumer-contracts.md`](./09-consumer-contracts.md) (Payments/Billing contracts).
