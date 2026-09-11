<!-- CONFLUENCE_TITLE: [BSS]: Orders Lifecycle — Design Decisions Register -->
<!-- Related: ./DESIGN.md, ./design/, ./ADR/ | Owners: BSS Orders team -->

# Design Decisions — Orders Lifecycle

<!-- toc -->

- [How to use this document](#how-to-use-this-document)
- [Status board](#status-board)
- [A. Foundational shape](#a-foundational-shape)
  - [D-01 (H) The engine owns every state change *(autonomous)*](#d-01-h-the-engine-owns-every-state-change-autonomous)
  - [D-02 (M) Slices live in `docs/design/`, accepting registry invisibility *(product-confirmed 2026-09-08)*](#d-02-m-slices-live-in-docsdesign-accepting-registry-invisibility-product-confirmed-2026-09-08)
  - [D-03 (H) Foundation plus seven capability slices *(autonomous)*](#d-03-h-foundation-plus-seven-capability-slices-autonomous)
  - [D-04 (H) The state machine is data, not control flow *(autonomous)*](#d-04-h-the-state-machine-is-data-not-control-flow-autonomous)
  - [D-05 (M) Refused attempts are audited *(autonomous, carries ADR-0005)*](#d-05-m-refused-attempts-are-audited-autonomous-carries-adr-0005)
- [B. Engine algorithm — resolves R-01…R-11](#b-engine-algorithm--resolves-r-01r-11)
  - [D-06 (H) Idempotency resolution precedes admissibility and the version check *(autonomous, fixes R-01)*](#d-06-h-idempotency-resolution-precedes-admissibility-and-the-version-check-autonomous-fixes-r-01)
  - [D-07 (H) The in-flight marker is upsert-and-reread *(autonomous, fixes R-02)*](#d-07-h-the-in-flight-marker-is-upsert-and-reread-autonomous-fixes-r-02)
  - [D-08 (H) Every refusal path audits, settles and commits *(autonomous, fixes R-03; carries ADR-0005)*](#d-08-h-every-refusal-path-audits-settles-and-commits-autonomous-fixes-r-03-carries-adr-0005)
  - [D-09 (H) Slice pre-checks become registered guards *(autonomous, fixes R-04)*](#d-09-h-slice-pre-checks-become-registered-guards-autonomous-fixes-r-04)
  - [D-10 (H) Slice writes become document contributions *(autonomous, fixes R-05)*](#d-10-h-slice-writes-become-document-contributions-autonomous-fixes-r-05)
  - [D-11 (H) Five transition rows are added *(autonomous, fixes R-07)*](#d-11-h-five-transition-rows-are-added-autonomous-fixes-r-07)
  - [D-12 (H) Rows disambiguated and the PRD's two `approved` edges restored *(autonomous, fixes R-08 and R-09)*](#d-12-h-rows-disambiguated-and-the-prds-two-approved-edges-restored-autonomous-fixes-r-08-and-r-09)
  - [D-13 (M) The spawn signal is permanent *(autonomous, fixes R-10)*](#d-13-m-the-spawn-signal-is-permanent-autonomous-fixes-r-10)
  - [D-14 (M) Draft auto-void targets `expired` and is called auto-void *(autonomous, fixes R-11; carries ADR-0004)*](#d-14-m-draft-auto-void-targets-expired-and-is-called-auto-void-autonomous-fixes-r-11-carries-adr-0004)
- [C. Event contract — resolves R-12…R-19](#c-event-contract--resolves-r-12r-19)
  - [D-15 (H) The event set stays at eleven; six row classes are event-less *(autonomous, fixes R-12, R-13, R-14, R-16; carries ADR-0004)*](#d-15-h-the-event-set-stays-at-eleven-six-row-classes-are-event-less-autonomous-fixes-r-12-r-13-r-14-r-16-carries-adr-0004)
  - [D-16 (H) Self-service acceptance is a fact in the submit commit, not a second event *(autonomous, fixes R-15; carries ADR-0004)*](#d-16-h-self-service-acceptance-is-a-fact-in-the-submit-commit-not-a-second-event-autonomous-fixes-r-15-carries-adr-0004)
  - [D-17 (M) The outbox has a re-drive operation *(autonomous, fixes R-19)*](#d-17-m-the-outbox-has-a-re-drive-operation-autonomous-fixes-r-19)
- [D. Schema — resolves R-20…R-36](#d-schema--resolves-r-20r-36)
- [E. Authorization — resolves R-37…R-42](#e-authorization--resolves-r-37r-42)
  - [D-31 (H) The permission matrix is exhaustive, and the placing party may not record acceptance *(autonomous, fixes R-37 and R-38)*](#d-31-h-the-permission-matrix-is-exhaustive-and-the-placing-party-may-not-record-acceptance-autonomous-fixes-r-37-and-r-38)
  - [D-32 (H) Delegation proof is a named, verifiable credential *(autonomous, fixes R-39)*](#d-32-h-delegation-proof-is-a-named-verifiable-credential-autonomous-fixes-r-39)
- [F. Ownership and inventory — resolves R-43…R-51 and R-74](#f-ownership-and-inventory--resolves-r-43r-51-and-r-74)
- [G. Non-functional posture — resolves R-52…R-67](#g-non-functional-posture--resolves-r-52r-67)
- [H. PRD fidelity](#h-prd-fidelity)
  - [D-56 (H) The deferred-activation instant is specified and a new seam ask is raised *(autonomous, fixes R-69)*](#d-56-h-the-deferred-activation-instant-is-specified-and-a-new-seam-ask-is-raised-autonomous-fixes-r-69)
  - [D-57 (M) The open-question register is reconciled row by row against PRD §15 *(autonomous, fixes R-70)*](#d-57-m-the-open-question-register-is-reconciled-row-by-row-against-prd-15-autonomous-fixes-r-70)
  - [D-58 (M) The operator outbox re-drive is a registered endpoint with no PRD basis *(autonomous)*](#d-58-m-the-operator-outbox-re-drive-is-a-registered-endpoint-with-no-prd-basis-autonomous)
  - [D-59 (M) PRD reason phrases are descriptors; the design owns the identifiers *(autonomous)*](#d-59-m-prd-reason-phrases-are-descriptors-the-design-owns-the-identifiers-autonomous)
  - [D-60 (M) A missing required line date is refused at the gate *(autonomous, closes Rcons-016, now carries ADR-0004)*](#d-60-m-a-missing-required-line-date-is-refused-at-the-gate-autonomous-closes-rcons-016-now-carries-adr-0004)
  - [D-61 (H) Re-approval after an amendment is a two-step seam interaction *(autonomous, supersedes D-12's mechanism)*](#d-61-h-re-approval-after-an-amendment-is-a-two-step-seam-interaction-autonomous-supersedes-d-12s-mechanism)
  - [D-62 (H) A third field class: commercial-frozen, for the two non-amendable axes *(autonomous)*](#d-62-h-a-third-field-class-commercial-frozen-for-the-two-non-amendable-axes-autonomous)
  - [D-63 (H) Two permission-matrix corrections against PRD §6.6 *(autonomous)*](#d-63-h-two-permission-matrix-corrections-against-prd-66-autonomous)
  - [D-64 (H) A draft carries version 1; submit appends version 2 *(autonomous)*](#d-64-h-a-draft-carries-version-1-submit-appends-version-2-autonomous)
  - [D-65 (H) Authorization precedes the idempotency probe; the probe precedes guard inputs](#d-65-h-authorization-precedes-the-idempotency-probe-the-probe-precedes-guard-inputs)
  - [D-66 (H) Both begin-fulfillment elections are policy rows with safe fallbacks *(autonomous)*](#d-66-h-both-begin-fulfillment-elections-are-policy-rows-with-safe-fallbacks-autonomous)
  - [D-67 (M) Every event carries a common order-summary block *(autonomous)*](#d-67-m-every-event-carries-a-common-order-summary-block-autonomous)
  - [D-68 (M) A cross-tenant read is refused as not-found, not forbidden *(autonomous)*](#d-68-m-a-cross-tenant-read-is-refused-as-not-found-not-forbidden-autonomous)
  - [D-69 (M) Adding a state or event type is additive; consumers must tolerate unknown values *(autonomous)*](#d-69-m-adding-a-state-or-event-type-is-additive-consumers-must-tolerate-unknown-values-autonomous)
  - [D-70 (M) The audit read is a design-introduced surface with no FR basis *(autonomous)*](#d-70-m-the-audit-read-is-a-design-introduced-surface-with-no-fr-basis-autonomous)
  - [D-71 (H) Acceptance is recordable on the partner path regardless of the required flag *(autonomous)*](#d-71-h-acceptance-is-recordable-on-the-partner-path-regardless-of-the-required-flag-autonomous)
  - [D-72 (H) Fail closed on an unevaluable gate input *(autonomous, now carries ADR-0003)*](#d-72-h-fail-closed-on-an-unevaluable-gate-input-autonomous-now-carries-adr-0003)
  - [D-73 (H) Every stored verdict records its deciding authority *(autonomous)*](#d-73-h-every-stored-verdict-records-its-deciding-authority-autonomous)
  - [D-74 (M) The per-line result is a projection, not a state machine *(autonomous)*](#d-74-m-the-per-line-result-is-a-projection-not-a-state-machine-autonomous)
- [I. Slice-local calls — each declared by its slice as warranting an entry](#i-slice-local-calls--each-declared-by-its-slice-as-warranting-an-entry)
  - [D-82 (M) The version reason vocabulary is `{create, submit, amendment}`](#d-82-m-the-version-reason-vocabulary-is-create-submit-amendment)
  - [D-83 (H) The in-flight order cap stays at one; route (b) does not resolve Q-05 *(carries `ADR/0007`)*](#d-83-h-the-in-flight-order-cap-stays-at-one-route-b-does-not-resolve-q-05-carries-adr0007)
  - [D-84 (H) One order line produces one subscription — Q-02 answered no](#d-84-h-one-order-line-produces-one-subscription--q-02-answered-no)
  - [D-85 (H) The cross-gear contract surface is GTS-typed *(closes the review's GTS findings)*](#d-85-h-the-cross-gear-contract-surface-is-gts-typed-closes-the-reviews-gts-findings)
  - [D-86 (H) The overlap collision is taken first, and detected as a row shortfall *(closes a CodeRabbit finding on PR #4775)*](#d-86-h-the-overlap-collision-is-taken-first-and-detected-as-a-row-shortfall-closes-a-coderabbit-finding-on-pr-4775)
  - [D-87 (H) A parked outbox row blocks its own order's stream and no other](#d-87-h-a-parked-outbox-row-blocks-its-own-orders-stream-and-no-other)
  - [D-88 (H) The idempotency key is scoped by authorized principal *(closes an IDOR finding)*](#d-88-h-the-idempotency-key-is-scoped-by-authorized-principal-closes-an-idor-finding)
  - [D-89 (M) The subscription axis of the overlap rule is disclosed as open, not bounded by a timed window](#d-89-m-the-subscription-axis-of-the-overlap-rule-is-disclosed-as-open-not-bounded-by-a-timed-window)
  - [D-90 (H) Bounded lifetime is a per-state TTL plus two re-entry caps, and the residual gap is disclosed](#d-90-h-bounded-lifetime-is-a-per-state-ttl-plus-two-re-entry-caps-and-the-residual-gap-is-disclosed)
- [Open questions](#open-questions)
- [Traceability](#traceability)

<!-- /toc -->

## How to use this document

Every call taken while authoring or repairing the design set lands here. An entry records the
**decision**, its **rationale**, and its **propagation** target — the sections that must agree
with it, addressed as `<doc> §<n>`. A wrong propagation address defeats every mechanised check
that could have caught a substantive drift, so the address is checked when the entry is written.

Items marked *(autonomous)* were decided by the authoring agent under a standing mandate —
decide where the call is technical. Items carrying product or cross-gear consequence are **not**
decided here: they are recorded in [Open questions](#open-questions) with a named owner.
Reopening a decision means flipping its status and recording why.

**Severity**: **[H]** breaks money or correctness, or is unimplementable as written · **[M]**
teams can build incompatible behaviour · **[L]** contained. Headings carry the severity in
parentheses rather than brackets, because a bracketed heading produces an unparseable
table-of-contents link.

**Two shapes.** An entry whose reasoning a reader needs in order to implement it correctly gets
its own section — the engine algorithm, the event contract, authorization and PRD fidelity are
written this way. An entry that is a determinate correction with a one-line reason gets a table
row in its area — schema, ownership and the non-functional baselines are written this way. Every
entry carries the same three fields either way; only the room given to the rationale differs.

**Status**: opened 2026-09-08. D-01…D-05 record calls taken during original authoring; D-06…D-57
resolve the 74 findings of the **2026-09-08 review wave** (`R-01`…`R-74`),
whose finding ids (`R-nn`) are cited per entry; D-58…D-60 come from the verification passes over
that remediation, which found two decisions asserted but not fully applied; D-61…D-90 come from
the 2026-09-09 and 2026-09-10 waves and from the review of PR #4775. **Thirty items are
routed as open questions** (`Q-01`…`Q-30`), of which twenty-six are routed and unanswered — Q-10
is out of this gear's scope, and Q-02 and Q-14 are closed by design decisions.

## Status board

| Area | Entries | Severity | State |
|------|---------|----------|-------|
| A. Foundational shape | D-01…D-05 | [H] ×3, [M] ×2 | decided; D-01 and D-03 carry ADRs, D-05 and D-08 carry ADR-0005 |
| B. Engine algorithm | D-06…D-14 | [H] ×7, [M] ×2 | decided (autonomous) — resolves R-01…R-11 |
| C. Event contract | D-15…D-17 | [H] ×2, [M] ×1 | decided (autonomous) — resolves R-12…R-19 |
| D. Schema | D-18…D-30 | [H] ×6, [M] ×7 | decided (autonomous) — resolves R-20…R-36 |
| E. Authorization | D-31…D-35 | [H] ×4, [M] ×1 | decided (autonomous) — resolves R-37…R-42 |
| F. Ownership and inventory | D-36…D-38 | [H] ×1, [M] ×2 | decided (autonomous) — resolves R-43…R-51, R-74 |
| G. Non-functional posture | D-39…D-55 | [H] ×10, [M] ×5, [L] ×2 | decided (autonomous, working baselines) — resolves R-52…R-67 |
| H. PRD fidelity | D-56…D-74 | [H] ×10, [M] ×9 | decided; §15 rows and PRD-wording asks routed to owners |
| I. Slice-local calls | D-75…D-90 | [H] ×8, [M] ×8 | decided; D-79 is a historical see-D-74 stub; **D-82…D-90 sit outside the area table**, each carrying its own full entry below |
| Open questions | Q-01…Q-30 | — | 26 routed and unanswered; Q-10 out of scope for this gear; Q-02, Q-13 and Q-14 closed without a Product decision (D-84; the PRD's own reason-phrase usage; a design correction) |

## A. Foundational shape

### D-01 (H) The engine owns every state change *(autonomous)*

**Decision**: one Order Transition Engine owns the aggregate, the append-only version chain, the
state-machine table, guard evaluation, the idempotency registry, the version check, the audit
store, the event outbox and the reason registry. Slices declare guards and supply document
contributions and never write order state.

**Rationale**: four `p1` NFRs are properties of how a state change commits, not of any
capability. Full alternatives analysis in [`ADR/0001`](./ADR/0001-cpt-cf-bss-orders-lifecycle-adr-transition-through-engine.md).

**Propagated**: `DESIGN.md §1.1`, `§2.1`; `01 §4.1`.

### D-02 (M) Slices live in `docs/design/`, accepting registry invisibility *(product-confirmed 2026-09-08)*

**Decision**: capability slices are authored at `docs/design/NN-*.md`, following the four BSS
sibling gears, rather than at `docs/features/` where the platform's registered FEATURE artifacts
live.

**Rationale**: consistency with `pricing`, `rating`, `subscriptions` and `ledger` outweighs the
tooling loss. The cost is explicit and accepted: `cfs where-used`, per-artifact validation and
`cfs spec-coverage` marker-to-code tracing do not reach these paths, so the slices are covered by
the repo-wide gate and nothing finer. Pricing operates this way with 490 source files.

**Propagated**: `design/README.md` preamble; `ADR/0002` More Information.

### D-03 (H) Foundation plus seven capability slices *(autonomous)*

**Decision**: the design set is a thin `DESIGN.md` index, a foundation slice, and seven
capability slices, all following the DESIGN template.

**Rationale**: the correctness core needs an independent review boundary — which demonstrably
worked. Alternatives in [`ADR/0002`](./ADR/0002-cpt-cf-bss-orders-lifecycle-adr-slice-decomposition.md).
Uniform DESIGN shape was chosen over pricing's mixed DESIGN/FEATURE shape because neither is
registry-indexed at these paths, so uniformity costs nothing and aids review.

**Propagated**: `DESIGN.md §1.3`, `§3.2`; `design/README.md`.

### D-04 (H) The state machine is data, not control flow *(autonomous)*

**Decision**: the machine is a transition table of rows; an edge that is not a row cannot be
taken; normative exclusions are expressed as absent rows.

**Rationale**: edge coverage becomes enumerable and testable, and the `in_fulfillment` expiry
exclusion becomes structural — a sweep defect produces a refusal rather than an orphaned order.

**Propagated**: `01 §3.2`, `§4.3`; `07 §3.6` *Sweep Expired Orders* step 2.4.4, `§4.3`.

### D-05 (M) Refused attempts are audited *(autonomous, carries ADR-0005)*

**Decision**: the audit store records refused transitions alongside committed ones, with an
`outcome` discriminator.

**Rationale**: a denied authorization or a failed guard that leaves no trace is invisible to a
reviewer, which defeats the point of a financial-grade audit trail.

**Propagated**: `01 §3.7` `orders_transition_audit`, `§4.4`; `08 §3.6` *Audit retrieval*.
**Consequence recorded in D-49**: unbounded refusal auditing is an amplification vector.

## B. Engine algorithm — resolves R-01…R-11

### D-06 (H) Idempotency resolution precedes admissibility and the version check *(autonomous, fixes R-01)*

**Decision**: the guard order becomes authorization → **idempotency resolution** → state-table
admissibility → version check → slice guards. A settled record whose request fingerprint matches
returns its stored outcome immediately. The version check applies only where no settled record
exists for the key.

**Rationale**: the original order was wrong and self-contradicting. Every versioning transition
bumps `current_version` on commit, so a retry of a committed submit or amendment carried a
superseded version and received version-conflict — never reaching the stored outcome. The replay
sequence and the `§4.2` outcome table both described the corrected order; only `§4.1`, `§2.1` and
the algorithm described the broken one. The justifying sentence in `§4.1` ("a replayed key against
a superseded version is a version conflict rather than a stored-outcome replay") was the error and
is deleted.

**Propagated**: `01 §2.1` (`principle-guard-declared-not-embedded`), `§3.6` steps 5–15, `§4.1`
¶2, `§4.2` table.

### D-07 (H) The in-flight marker is upsert-and-reread *(autonomous, fixes R-02)*

**Decision**: the in-flight insert becomes insert-if-absent followed by a re-read that branches to
the settled or in-flight case. The claim that the marker is "a committed row, so a concurrent
duplicate observes it" is withdrawn.

**Rationale**: inserted and settled inside one transaction, the marker is never visible to a
concurrent caller. A duplicate blocked on the unique index and then received a violation that had
already aborted its own transaction, making a plain return impossible — and by then the first
attempt had settled *successfully*, so the correct answer was the stored success, not
still-processing. `still-processing` was unreachable and a crashed request left no marker at all.

**Propagated**: `01 §1.2` (`nfr-order-idempotency`), `§3.6` steps 13–14, `§4.2` outcome table.

### D-08 (H) Every refusal path audits, settles and commits *(autonomous, fixes R-03; carries ADR-0005)*

**Decision**: each refusal class appends its audit entry, settles the idempotency record where one
exists, and commits before returning.

**Rationale**: `§4.1` required exactly this and the algorithm delivered it for one refusal class
in five, so the 100 % audit NFR was unmet by the algorithm meant to guarantee it.

**Propagated**: `01 §3.6` *Attempt Transition* steps 8, 11, 12, 13, 15, `§4.1`.

**The one exception is now closed.** *Attempt Transition* step 3 refuses an unresolvable guard
input before step 4 opens the ordinary transaction, which once left the most frequent refusal in
the system — [`ADR/0003`](./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md)'s
fail-closed posture makes it so — unaudited and unsettled. Step 3.1 now **opens a refusal
transaction of its own**, taking the row lock, resolving or creating the idempotency record,
settling it with the guard's registered unevaluable reason, appending the refused-attempt audit
entry and committing. The open finding of review wave 4 (`F2-SEM-007` / `Rc2-025`) is resolved,
and "without exception" holds as written; the only remaining scoping caveat is authorization
denial, which audits and commits but deliberately does not settle a caller-supplied key
(`01 §4.1`, [`ADR/0005`](./ADR/0005-cpt-cf-bss-orders-lifecycle-adr-refusals-commit.md)).

### D-09 (H) Slice pre-checks become registered guards *(autonomous, fixes R-04)*

**Decision**: checks a slice performed before calling the engine become guard predicates
registered against the transition row and evaluated by the engine.

**Rationale**: a refusal raised before the engine is called produces no audit row and no
idempotency record, so a refused submit was neither auditable nor replayable — and `01 §2.1`
already required guards to be declared and engine-evaluated.

**Propagated**: `03 §3.6`, `04 §3.6`, `05 §3.6`, `06 §3.6`, `07 §3.6`.

### D-10 (H) Slice writes become document contributions *(autonomous, fixes R-05)*

**Decision**: durable rows a slice wrote before requesting the transition — verdict, linkage,
projection, acceptance instant, pre-hold state, gate outcome — are passed as contributions to the
engine call and written inside its transaction.

**Rationale**: writing then transitioning leaves an orphaned row on refusal, defeating "no partial
commit to reconcile" and contradicting the single-writer constraint.

**Propagated**: `03 §3.6` *Run Gate and Submit* step 11.1; `05 §3.6` *Record Acceptance* step 5;
`06 §3.6` *Acknowledge Fulfillment* steps 2.1.2, 2.2.2, 3, 4;
`07 §3.6` *Hold Then Resume* step 2.

### D-11 (H) Five transition rows are added *(autonomous, fixes R-07)*

**Decision**: rows are added for order creation (`∅ → draft`), draft content mutation
(`draft → draft`, state-only), the administrative edit (any non-terminal → same state,
state-only), the spawn-signal report (`in_fulfillment → in_fulfillment`, state-only) and draft
auto-void (`draft → expired`, actor class `system`). The table is twenty-five rows.

**Rationale**: five operations the slices specify had no row, and the engine refuses any
`(state, trigger)` without one — so the whole of Phase 1 was inadmissible under the design's own
machine.

**Propagated**: `01 §4.3`; `02 §1.2`, `§3.6`; `04 §3.6`, `§4.1`; `06 §3.3`, `§4.1`, `§4.3`;
`07 §3.2`, `§4.4`; `design/README.md` authoring status.

### D-12 (H) Rows disambiguated and the PRD's two `approved` edges restored *(autonomous, fixes R-08 and R-09)*

**Decision**: the in-place amendment row is restricted to `submitted` and `pending_approval`. The
amendment-from-`approved` row is **split into the PRD's two guarded edges** —
`approved → submitted [approval not required for the new version]` and
`approved → pending_approval [approval required]` — with the requirement verdict as the guard.
Each row carries exactly one versioning behaviour.

**Rationale**: two rows matched `(approved, amendment)`, making the lookup non-deterministic. The
original collapse also narrowed a PRD edge and `04 §4.3` then committed to `submitted`
unconditionally, leaving the `pending_approval` target unreachable — a scope change presented as
an implementation detail. Restoring the PRD's edges removes the need for a scope-change approval
entirely, which is why this is decidable here rather than routed.

**Propagated**: `01 §4.3` rows; `04 §1.1`, `§2.2`, `§3.6` step 11, `§4.3`.

**Superseded in part by D-61**: the two-row split stands as a description of
the PRD's declared edges, but the requirement verdict is no longer their guard — nothing in this
gear can obtain a verdict for an uncreated version. Rows 19 and 20 are now unguarded amendment
rows targeting `submitted`.

### D-13 (M) The spawn signal is permanent *(autonomous, fixes R-10)*

**Decision**: `spawn_signal_at` is written once and never cleared. The clearing rule is deleted.

**Rationale**: the rule cleared it "only by a workflow-mediated cancel", but that cancel lands in
`cancelled`, which is terminal with no row out — so no subsequent attempt exists and the rule was
unreachable.

**Propagated**: `06 §3.7`, `§4.3`; `01 §1.2` (`fr-order-cancel` row), `§3.7`.

### D-14 (M) Draft auto-void targets `expired` and is called auto-void *(autonomous, fixes R-11; carries ADR-0004)*

**Decision**: an abandoned draft transitions `draft → expired` with actor class `system`, reusing
`OrderExpired`. The outcome is called **auto-void** everywhere; "archived" and "expired" as
descriptions of it are removed. Auto-void writes no destructive path — the order and its trail
remain readable in the terminal state.

**Rationale**: the sweep targeted `archived`, a state in neither the state set nor the terminal
set, with no row; and the outcome was named three ways across the set. Reusing `expired` avoids a
twelfth state and a twelfth event.

**Retires**: archived, archival, archive — except: WAL archiving, archival tier, retention tier, is not a state, only term used, no `archived` state

**Propagated**: `01 §4.3`; `02 §1.2`, `§4.5`; `07 §3.2`, `§4.4`, `§3.7`; `DESIGN.md §1.2`.

## C. Event contract — resolves R-12…R-19

**Not in the PRD's diagram.** PRD §6.1's normative state machine contains no `draft → expired` edge; §7.1 only says abandoned drafts *should* be auto-voided rather than deleted. Reusing `expired` avoided a twelfth state and a twelfth event, but it means `OrderExpired` now carries two commercially different facts — a committed order whose TTL lapsed, and a basket never submitted. The payload distinguishes them, so the gap is disclosure rather than correctness; flagged here as an addition to a PRD-normative diagram needing Product's acknowledgement, the treatment D-58 already applies.

### D-15 (H) The event set stays at eleven; six row classes are event-less *(autonomous, fixes R-12, R-13, R-14, R-16; carries ADR-0004)*

**Decision**: the transition table gains a nullable `event_type` column. `§4.4` becomes "exactly
one outbox row per committed transition **where the row declares an event type**". The eleven PRD
events map to rows as follows, and six row classes are deliberately event-less:

| Event | Emitting rows |
|-------|---------------|
| `OrderSubmitted` | `draft → submitted` |
| `OrderApproved` | `submitted → approved`, `pending_approval → approved` |
| `OrderRejected` | `pending_approval → rejected` |
| `OrderAmended` | both amendment rows and the in-place amendment |
| `OrderHeld` | `* → on_hold` |
| `OrderResumed` | `on_hold → pre-hold state` |
| `OrderCancelled` | every cancel row |
| `OrderExpired` | both TTL expiry rows and draft auto-void |
| `OrderCompleted` | `in_fulfillment → completed` |
| `OrderFulfillmentFailed` | `in_fulfillment → fulfillment_failed` |
| `OrderAcceptanceRecorded` | the acceptance row, partner path only (see D-16) |

**Event-less rows**: `submitted → pending_approval`, `approved → in_fulfillment`, creation, draft
content mutation, the administrative edit, and the spawn-signal report.

**Rationale**: "the eleven state events" was asserted four times and enumerated nowhere, four
were never named, and `event_type` was read by the algorithm from a tuple that did not declare
it. The event-less rows are safe rather than a gap: in each case the **caller caused the
transition and already knows** — the sibling gear reflects the verdict and calls
begin-fulfillment, and its trigger set (PRD §6.1 of the Workflow PRD) contains neither event.
Adding event types would have been a PRD scope change; this resolution needs none.

**Propagated**: `DESIGN.md §1.2`, `§1.3`, `§3.3`; `01 §3.2`, `§3.6` step 20, `§3.7`
`orders_event_outbox`, `§4.3`, `§4.4`; `04 §3.6`, `§4`.

### D-16 (H) Self-service acceptance is a fact in the submit commit, not a second event *(autonomous, fixes R-15; carries ADR-0004)*

**Decision**: on the self-service path the acceptance instant is written by the submit
contribution inside the submit commit, and **only `OrderSubmitted` is published**, carrying the
acceptance instant in its payload. `OrderAcceptanceRecorded` is published only on the
partner-placed path, where recording is a separate transition.

**Rationale**: the original text mandated publishing both events "within the submit commit",
which one commit cannot do — it cannot be two transition rows, and it cannot enqueue two outbox
rows without breaking the one-row invariant and the `(order_id, sequence)` unique constraint. The
chosen resolution keeps the PRD's rule that self-service submit *constitutes* acceptance with no
separate field, and keeps the event set intact.

**Propagated**: `05 §1.2`, `§3.6` *Self-service*, `§4.2`; `01 §4.4`.

### D-17 (M) The outbox has a re-drive operation *(autonomous, fixes R-19)*

**Decision**: a parked dead-letter row may be re-driven by an operator-invoked operation that
re-publishes under the original event id, so consumer de-duplication makes replay safe. Re-drive
is audited and restricted to the seller-operator actor class.

**Rationale**: a parked row was alerted and "inspectable" with no recovery path, and the sibling
gear's reconciliation sweep is read-only past the idempotency window — so a permanently parked
`OrderCompleted` left the two gears divergent forever.

**Propagated**: `01 §3.6` *Drain Outbox*, `§3.7` `orders_event_outbox`, `§4.4`; `08 §4.3`.

## D. Schema — resolves R-20…R-36

| ID | Sev | Decision | Rationale | Propagated |
|----|-----|----------|-----------|-----------|
| D-18 | [H] | `orders_resolved_total` gains `scope enum('line','order')` in the primary key; `line_id` is non-null for line rows and a zero UUID for the roll-up | A nullable column cannot participate in a primary key, so the order-level roll-up row the design requires was unstorable (R-20) | `01 §3.7`; `03 §4.4` |
| D-19 | [H] | Draft content lives in mutable working tables that submit materialises into version 2; administrative content lives in mutable `orders_order_admin` and `orders_order_line_admin` tables | Draft "free modification" and the in-place administrative edit both wrote tables declared append-only, so two specified paths violated their own constraints (R-21, R-22, R-26) | `01 §3.7`, `§2.1`, `§4.3`; `02 §3.7`, `§4.1`, `§4.3`; `04 §3.6`, `§3.7` |
| D-20 | [H] | The foreign-key graph is declared: every child references `order_id`, version-scoped children reference `(order_id, version)`, and the `orders_order.current_version` cycle is a deferred constraint | No foreign key existed across twelve tables while the design claimed a recovered database could not hold a state change without its trail (R-23) | `01 §3.7` |
| D-21 | [H] | `orders_order_line_identity(order_id, line_id)` is added as the parent of order-scoped line identity; `orders_line_fulfillment` and `orders_resolved_total` reference it, and the projection gains `version` | `line_id` was claimed unique within an *order* while the PK enforced uniqueness within a *version*, and two tables keyed on the uniqueness nothing provided (R-24, R-34) | `01 §3.7`; `02 §3.7`, `§2.1` |
| D-22 | [H] | Ten columns are added to the canonical schema: the tolerated-authorization risk flag, `state_entered_at`, the per-line date policy-switch state, audit `changed_field`/`prior_value`/`new_value`, line `currency`, `overlap_scope_key`, the hold actor/instant/reason, and compensation evidence | Slices required each of them normatively and the schema calling itself canonical defined none (R-25) | `01 §3.7`; `02 §3.7`; `04 §3.7`; `05 §3.7`; `06 §3.7`; `07 §3.1`, `§3.7`; `08 §3.7` |
| D-23 | [M] | Indexes are matched to the declared query paths: `(resource_tenant_id, state, state_entered_at)` and `(seller_tenant_id, state, state_entered_at)` replace the payer composite and serve the scoped lists and the sweeps alike, `(contract_id)` and `expires_at` are added, and the outbox gains a partial index on undelivered rows plus a purge policy for delivered ones | The one composite served no path any slice described, the partner path and contract filter were unindexed, and the drain scan grew with every event ever emitted (R-27) | `01 §3.7`; `07 §3.6`; `08 §3.7` |
| D-24 | [M] | `sequence` is allocated from a counter column on `orders_order` incremented under the aggregate row lock the transition already takes at *Attempt Transition* step 5 | `MAX+1` was implied, which serialises writes and surfaces concurrency as unique-violation faults instead of the version-conflict refusal the contract promises (R-28) | `01 §3.6` *Attempt Transition* step 5, `§3.7` |
| D-25 | [M] | Statements not expressible as DDL are relabelled **engine-enforced invariants** with a named verification test; "constraint" is reserved for genuine DDL | Five items called constraints require cross-table or cross-row conditions, or express a writer, which no constraint can (R-29) | `01 §3.7` |
| D-26 | [H] | *(carries [`ADR/0007`](./ADR/0007-cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency.md))* `overlap_scope_key` is persisted on the line and `orders_inflight_overlap_claim` enforces one open payer/key claim with a partial unique index inside the transition transaction; the gate predicate remains as the friendly pre-check | The rule was resolved outside the transaction with no constraint behind it, so two concurrent identical submits both passed (R-30); root state cannot be indexed from an append-only line, so the claim table owns the mutable lifecycle | `01 §3.7`; `03 §4.2` predicate 9 |
| D-27 | [M] | `order_market` moves to `orders_order_version` | Stored on the root, an amendment overwrote the market the prior version was gated against, and the activation re-check had no "market frozen at submit" left to compare (R-31) | `01 §3.1`, `§3.7`; `03 §3.1`, `§3.6`, `§3.7`; `04 §4.2` |
| D-28 | [M] | `orders_state_ttl_policy` uses `NULLS NOT DISTINCT`; `orders_approval_reflection` is UNIQUE on `(order_id, version, verdict_kind)` | SQL treats NULLs as distinct, so duplicate platform policies were possible; and one approval-required version needs both a requirement verdict and a later gate outcome, while each kind must still exclude contradictory values (R-32, R-33) | `07 §3.7`; `06 §3.7` |
| D-29 | [M] | `order_market` becomes `market_currency` + `market_region` columns; `catalog_price_pin` declares its fields | Both were opaque `jsonb` while a gate predicate must filter on the market and the pin is the object the resolvability invariant is asserted over (R-35) | `01 §3.7`; `03 §4.3` |
| D-30 | [M] | A migration and schema-versioning subsection is added, plus the auto-void terminal as the archive posture | No migration strategy existed and archival was asserted with no target (R-36) | `DESIGN.md §3.7`; `01 §3.7` |

## E. Authorization — resolves R-37…R-42

### D-31 (H) The permission matrix is exhaustive, and the placing party may not record acceptance *(autonomous, fixes R-37 and R-38)*

**Decision**: the matrix declares every operation across all slices, and gains a normative rule:
**on a partner-placed order the actor who created or submitted it may not record the customer's
acceptance instant.** Recording requires a principal of the resource-tenant party; Seller
Operator has no recording authority because no verifiable customer-instruction artifact exists.

**Rationale**: thirteen operations had no declaration, so on the design's own startup rule the
gear could not start — and among them was the acceptance operation, leaving nothing to prevent
exactly the conflation `05 §2.1` was written to prevent: a partner's own authority offered as
proof of their customer's consent. This is the one review finding that was a substantive hole
rather than a documentation gap.

**Propagated**: `08 §3.2`, `§4.3`; `05 §2.1`, `§3.6`, `§4.1`, `§4.2`.

### D-32 (H) Delegation proof is a named, verifiable credential *(autonomous, fixes R-39)*

**Decision**: delegation proof is a signed assertion issued by Account Management naming the
delegating tenant, the delegated scope, the delegate, an issue instant and a finite expiry,
verified against Account Management's issuer key at the pre-guard, and revocable by the
delegating tenant with revocation checked at verification. Its reference is recorded on the audit
entry in a new column, and the design cites BSS manifest §2.1.3 as the PRD requires.

**Rationale**: the single control preventing cross-tenant leakage had no form, issuer, trust
anchor, validation rule, lifetime or revocation, the algorithm tested a "valid" proof with
validity undefined, the PRD's normative pointer appeared nowhere, and the audit table had no
column to hold what `§4.4` said must be recorded.

**Propagated**: `08 §2.2`, `§3.6`, `§4.4`; `01 §3.6` *Attempt Transition* step 1, `§3.7` `orders_transition_audit`.

| ID | Sev | Decision | Rationale | Propagated |
|----|-----|----------|-----------|-----------|
| D-33 | [H] | The Workflow-only operations require a gateway-asserted service principal plus a scope claim naming this gear; the pre-guard checks both. Actor class alone is insufficient | Nothing distinguished the sibling gear from any caller presenting that actor class (R-40) | `06 §4.1`; `DESIGN.md §4`; `01 §3.6` *Attempt Transition* step 1 |
| D-34 | [H] | One authorization evaluator is extracted and invoked by both the engine pre-guard and the read paths | The engine's only entry is *attempt a transition*, so reads necessarily implemented a second model — the drift `08 §2.1` claims to prevent (R-41) | `08 §2.1`, `§3.2`, `§4.3`; `01 §3.3` |
| D-35 | [M] | A read access log is defined as a separate append-only surface recording reads and refused reads with the delegation-proof reference; `§4.4`'s claim is narrowed to point at it | Reads register no transition and only the engine may write the audit store, so no record of a read could exist (R-42) | `08 §3.7`, `§4.4` |

## F. Ownership and inventory — resolves R-43…R-51 and R-74

| ID | Sev | Decision | Rationale | Propagated |
|----|-----|----------|-----------|-----------|
| D-36 | [H] | The ordinary cancel operation is assigned to `07-hold-and-expiry`, which already owns the cancel-from-`on_hold` guard, with its algorithm, guard set and registered reasons | No slice owned it: three transition rows and three actor permissions depended on an operation with no algorithm, no guards and no reasons (R-43) | `07 §3.3`, `§3.6`, `§4`; `DESIGN.md §3.3`; `design/README.md` |
| D-37 | [M] | `DESIGN.md §3.7` becomes the complete gear-table inventory with one ownership rule — **engine owns schema and writes, slice owns content** — and `§3.3` becomes the union of the slice endpoint surfaces | Ownership was assigned twice incompatibly, the inventory omitted the tables slices introduce, and the endpoint inventory omitted eight endpoints while declaring one nobody owned (R-44, R-45) | `DESIGN.md §3.3`, `§3.7`; `01 §3.7` |
| D-38 | [M] | One reason name per condition: the engine's `version-conflict` replaces `stale-version`, `version-stale` and `verdict-version-stale`; `commercial-field-immutable` replaces the two variants; `expiry-not-permitted-for-state` is deleted in favour of the engine's `not-admissible`; `administrative-field-in-amendment` is registered by `04 §3.3` for a delta naming an administrative field, distinct from capture's `commercial-field-immutable` and resolved ahead of `tenant-axis-immutable` by `01 §4.1`'s registration order. Per-entity IDs are minted, the duplicate sequence ID is removed, the dependency table gains four edges, and all seven count inconsistencies are corrected | Callers key on reason strings, so three names for one condition is a contract defect; and derived facts had drifted across ten documents (R-46…R-51, R-74) | `01 §3.3`; `02 §3.3`; `04 §3.3`; `06 §3.3`; `07 §3.3`; `DESIGN.md §3.1`, `§3.6`; `design/README.md` |

## G. Non-functional posture — resolves R-52…R-67

All values below are **working baselines** pending the program-wide NFR workshop, consistent with
the PRD's own posture on its latency and retention thresholds. They are recorded as decisions
rather than left blank because a threshold nobody set is a threshold nobody can verify against.

| ID | Sev | Decision | Rationale | Propagated |
|----|-----|----------|-----------|-----------|
| D-39 | [H] | The idempotency-key window is **24 hours**, matching the sibling catalog gear's ratified value, and must exceed the sibling Workflow gear's reconciliation-sweep horizon | The PRD assigns Design a `MUST` to set a finite window; the design restated the obligation, set nothing, and omitted it from both open-value registers (R-68) | `01 §2.2`, `§3.7`; `07 §4.5` |
| D-40 | [H] | TCV **arrives computed** from the price-evaluation contract and is stored verbatim; the formula in `03 §4.4` is marked as reproduced from the PRD glossary for the reader, not as an instruction to this gear | A normative multiplication and annealisation formula sat against `constraint-no-money-arithmetic` and R4's prohibition, with the result persisted and no statement of who evaluated it — a compliance question, not a wording one (R-71) | `03 §4.4`; `DESIGN.md §2.2`, `§1.2`; `03 §1.1`, `§1.2`, `§3.2` |
| D-41 | [H] | *(carries [`ADR/0006`](./ADR/0006-cpt-cf-bss-orders-lifecycle-adr-outbox-publication.md))* Capacity baselines: 50 order transitions/second peak, 200 outbox events/second drain throughput, ~12 rows per order at version 1 and ~6 per amendment, archival tier triggered at 24 months past terminal. The **event-delivery budget is 30 seconds p95**, matching the PRD's process-event latency class | No capacity model existed while the PRD says thresholds "apply at production load (sizing in Design)", and the event-delivery budget was cited four times as a verification and alerting target and defined nowhere (R-52, R-53) | `DESIGN.md §4`; `01 §1.2`, `§3.8` |
| D-42 | [H] | Per-port deadlines (250 ms catalog, 250 ms identity, 500 ms evaluation, 250 ms overlap, 250 ms contracts, 250 ms Preview-only tax) inside a **1.5 s submit** budget over the five submit-path ports and a **1.75 s Preview** budget over all six; bounded retry with two attempts on transient failure only; a breaker per port opening on a rolling failure ratio and mapping to that port's existing fail-closed reason; a concurrency bulkhead per port; and rate limits declared in the operation specs. The outbox drain is **sharded by `order_id` hash across N leases and publishes in batches** | Only unavailable ports were handled and nothing specified a slow one, while five synchronous calls sat on the request path; and the single-leaseholder unbatched drain capped gear event throughput regardless of replica count (R-54, R-55) | `03 §2.1`, `§3.3`; `08 §3.5`; `01 §3.6`, `§3.8` |
| D-43 | [H] | Synchronous commit to a quorum with one standby in a second failure domain **inside** the residency boundary; nightly base backup with continuous WAL archiving for point-in-time recovery; RTO ≤ 60 min met by standby promotion, with a named DR drill each release | RPO zero and RTO ≤ 60 min were asserted with one mechanism that constrained nothing about surviving loss of the primary, and `§4` referred to "DR replicas" never specified (R-56) | `DESIGN.md §3.8`, `§4`; `01 §3.8` |
| D-44 | [H] | Data protection: encryption at rest by the platform's storage layer, TLS in transit on every hop, keys held in the platform KMS with the gear holding none, order content classified **commercial-confidential** and actor identifiers **personal-minimal**, no masking requirement since no surface returns another tenant's data, and erasure satisfied by pseudonymising actor identifiers in place — the one permitted mutation of the audit store, itself audited | The whole of data protection returned zero hits and the only explicit non-applicability in the set was PCI DSS, which the checklist's evidence standard treats as a violation rather than an exemption (R-57) | `DESIGN.md §4` |
| D-45 | [H] | A threat table is added covering the three tenancy axes, the partner-placed path, the Workflow-only operations, the outbound ports and the Preview surface, each with vector, boundary crossed, mitigation and residual risk | There was no threat model — one sentence naming one threat — while the threats the design named elsewhere were never mapped to mitigations (R-58) | `DESIGN.md §4` |
| D-46 | [H] | The audit store gains a predecessor-hash column forming a per-order chain, plus explicit revocation of UPDATE and DELETE on the audit role; the chain is verified by a periodic job | The PRD requires a tamper-**evident** record; `(order_id, sequence)` uniqueness detects nothing, and "append-only" was a property with no enforcement while the runtime holds database privilege (R-59) | `01 §3.7`, `§4.4`; `DESIGN.md §1.2` |
| D-47 | [H] | Each slice gains an observability subsection naming its own metrics, log fields and alerts; latency-SLO alerts are added for both budgets with a burn-rate policy; the tracing propagation contract across the seam and the ports is stated; readiness and liveness are distinguished | Observability was specified once, at gear level, entirely around engine-owned signals, so the risky work was unmonitored — and the alert list contained no alert on either latency SLO (R-60, R-65) | `DESIGN.md §4`; every slice `§3.8` |
| D-48 | [H] | An "Extension points and stability" section is added to `01` naming what a slice may add without an engine change — guards, reasons, contributions, policy rows — versus what requires one: a state, an edge, an event type or a schema column. `DESIGN.md §3.3` gains an API-evolution subsection with the stability ladder, the breaking-change definition and a deprecation window | The engine is deliberately closed and the design never said so, while all endpoints were uniformly "unstable" with no promotion criterion and the PRD delegates the major-version mechanism to Design (R-61) | `01 §4.6`; `DESIGN.md §3.3` |
| D-49 | [M] | Refusal audit rows carry a 90-day retention distinct from committed transitions, and repeated refusals against one order are rate-limited | Unbounded refusal auditing let any caller grow the audit store and slow every audit read on that order (R-62) | `01 §3.7`; `08 §4.5` |
| D-50 | [M] | The billing-chain tax owner is declared as a fifth outbound port with its own unavailability reason; Preview declares its actor classes and a rate limit | The tax owner appeared in no dependency table while the slice stated there were exactly four ports, and Preview took no security context and carried no rate limit (R-63) | `03 §3.3`, `§3.5`, `§4.6`; `DESIGN.md §3.5`; `08 §4.3` |
| D-51 | [M] | Replica reads are **forbidden**; the no-replication-lag claim stands because the read is the aggregate row | `§3.8` permitted replica reads while `§2.2` and `§4.1` forbade stale answers, and a lagging replica answers successfully with old state and nothing detects it (R-64) | `08 §1.2`, `§3.8` |
| D-52 | [M] | Preview **persists** its gate outcomes, with a 7-day retention and a rate limit; the three "creates no state" claims are corrected to "creates no order" | Preview was specified as writing nothing and as writing a row per predicate per line with its own retention; persistence is worth keeping for support, so the claims are what changes (R-51) | `03 §3.6`, `§3.7`, `§4.6`; `DESIGN.md §3.2` — **and is a PRD §9.1 deviation**: §9.1 specifies Preview as creating and mutating **no state**, while it persists a gate-outcome row per predicate per line under a 7-day retention. Needs Product's acknowledgement alongside D-58 (D-70 routing) |
| D-53 | [M] | The platform-inherited IaC posture is stated, and the unchosen policy values are delivered as `orders_state_ttl_policy` rows and gear configuration promoted through environments with the deployment | IaC was neither addressed nor marked inapplicable, and "no code default" had no stated delivery path (R-66) | `DESIGN.md §3.8` |
| D-54 | [L] | Residency is promoted to `constraint-data-residency`; vendor/licensing and resource constraints are marked explicitly inapplicable; the two **Location** fields gain repository paths | Residency was prose with no constraint ID, two checklist categories were neither present nor excluded, and no machine-readable contract was linked anywhere (R-67) | `DESIGN.md §2.2`, `§3.1`, `§3.3` |
| D-55 | [L] | `actor-orders-contracts` is cited in the gate and preconditions sequences, and the operation count is corrected to thirteen | One of the PRD's eight actors was referenced nowhere, and the count was wrong (R-72, R-73) | `DESIGN.md §3.3`, `§3.6`; `03 §3.6`; `05 §3.6` |

## H. PRD fidelity

### D-56 (H) The deferred-activation instant is specified and a new seam ask is raised *(autonomous, fixes R-69)*

**Decision**: the activation intent carries the **actual activation instant** as the subscription
start, and the quoted service-activation date separately as the requested date. A new upstream
ask, **`SUB-O10`**, is raised on the Subscriptions gear: `create` and the activation intent must
accept an explicit start instant and must not derive it from any date on the order. The order
read and Preview continue to show the per-line deferral.

**Rationale**: the PRD requires that a line deferred past its quoted date produce a subscription
whose start is the actual activation instant, never backdated. The design carried one rationale
clause and no mechanism — no obligation on the intent, no port contract, no seam ask — although
Subscriptions owns the start, so the requirement was unenforceable from this side.

**Propagated**: `02 §4.2`; `03 §4.6`; `06 §3.3`, `§4.3`, `§4.6`; `07 §4.3`; `08 §4.2`;
`UPSTREAM_REQS.md`.

### D-57 (M) The open-question register is reconciled row by row against PRD §15 *(autonomous, fixes R-70)*

**Decision**: every place the design speaks about open questions now cites the PRD §15 row it
means. Three rows the design ignored are recorded as explicit deferrals with their PRD owners
(see Q-01, Q-02, Q-03). `07 §4.5` is split into PRD-owned questions and design-owned values, and
the 24-hour overdue window is recorded as a **committed PRD default**, not an open question.

**Rationale**: the design tracked a different register than the PRD — claiming twenty unanswered
where §15 has fifteen rows and twelve unanswered, with three of the four it named not being §15
rows at all. Two ignored rows were additionally foreclosed by schema choices made without
reference to them.

**Propagated**: `DESIGN.md §4`; `02 §4.2`; `03 §4.5`; `07 §4.5`; `08 §4.5`.

### D-58 (M) The operator outbox re-drive is a registered endpoint with no PRD basis *(autonomous)*

**Decision**: the re-drive introduced by D-17 is added to `DESIGN.md §3.3` as the twenty-fifth
endpoint, owned by `foundation`, and the §3.3 prose separates the eleven endpoints the PRD
describes in §6 but omits from §9.1 from this one, which the PRD does not describe at all. It is
labelled a design-introduced operational surface needing Product's acknowledgement.

**Rationale**: the operation had an authorization declaration in `08 §4.3` and an interface in
`01 §3.3` but appeared in no endpoint inventory, so the design both claimed its endpoint table was
the union of every surface and claimed the permission matrix was exhaustive over that table, while
one authorized operation sat outside both. Folding it in silently would have presented a
design-introduced endpoint as a PRD requirement.

**Propagated**: `DESIGN.md §3.3`; `01 §3.3`; `08 §4.3`, `§2.2`, `§3.2`; `design/README.md`.

### D-59 (M) PRD reason phrases are descriptors; the design owns the identifiers *(autonomous)*

**Decision**: the PRD's reason phrases are read as **descriptors of a condition**, not as literal
reason names, and `01 §4.2` records the descriptor-to-identifier mapping. Where a descriptor is
already a good identifier it is adopted verbatim; `stale-version` is not, and resolves to the
engine's `version-conflict`.

**Rationale**: PRD §6.2 and §12 require "a machine-readable **stale-version** reason", which D-38
renamed without acknowledging the divergence — leaving a `MUST` apparently unmet. The PRD uses the
same running-prose construction for reasons it plainly does not name ("a machine-readable
business-level reason code"), so the phrases describe conditions rather than mint identifiers. The
mapping is recorded so a later reader does not restore a descriptor as a name and reintroduce the
duplication D-38 removed.

**Retires**: verdict-version-stale, version-stale, commercial-field-immutable-outside-amendment

**Propagated**: `01 §3.2` reason registry, `§4.2`; `04 §3.3`; `06 §3.3`.

### D-60 (M) A missing required line date is refused at the gate *(autonomous, closes Rcons-016, now carries ADR-0004)*

**Decision**: where a line's required service-activation or customer-acceptance date is absent,
submit is **refused at the gate** with a machine-readable reason. The order stays in `draft` and
the caller supplies the date. The rejected alternative was a twelfth order state — a waiting state
for orders whose dates are incomplete.

**Rationale**: a twelfth state would carry its own TTL, its own permitted edges and its own
event, for a condition that is a missing field rather than a commercial position. Refusing at the
gate keeps the state machine at eleven and makes the omission visible where every other
sellability failure is visible. `02 §1.2` cited this decision as `D-11`, which is about
transition rows and does not cover it — this entry is its real home, and resolves **PRD §15 row
8** (owner: Product with Design).

Full alternatives analysis in
[`ADR/0004`](./ADR/0004-cpt-cf-bss-orders-lifecycle-adr-closed-enumerations.md), which
consolidates this decision with D-14, D-15 and D-16 as one closed-enumerations decision.

**Propagated**: `02 §1.2`, `§4.2`; `03 §4.2`.

### D-61 (H) Re-approval after an amendment is a two-step seam interaction *(autonomous, supersedes D-12's mechanism)*

**Decision**: an amendment from `pending_approval` or `approved` transitions the order to
`submitted` and publishes `OrderAmended`; the sibling gear then obtains the requirement verdict
for the new version and reflects the order onward through the existing rows 7 and 8. Rows 18, 19
and 20 carry **no verdict guard**. The reason `verdict-unavailable` is deleted.

**Rationale**: D-12 split the amendment-from-`approved` edge into two rows guarded by "the new
version's requirement verdict" — a guard input nothing in the specified system can supply.
Verdicts exist only as reflections keyed `(order_id, version)`, so none can exist for a version
the amendment has not yet created; `06 §4.2` forbids deriving one from the superseded version;
no port to the approval policy owner is declared; and PRD §12 AC-11a forbids this gear to query
that owner. The guard therefore failed always, rows 19 and 20 were unreachable, and amendment
from `approved` was impossible — while an amendment in `pending_approval` whose new version no
longer required approval had no exit at all, since row 8 admits that verdict only from
`submitted`. The two-step shape needs no new port, no PRD amendment to AC-11a, and no caller-
supplied verdict (which would have breached R2). Its cost is one extra transition and a
divergence from the PRD's *direct* `approved → pending_approval` edge, routed as Q-12.

**Retires**: verdict-unavailable

**Propagated**: `01 §4.3` rows 18-20 and exclusions; `04 §3.3`, `§3.6` step 11, `§3.8`, `§4.3`;
`06 §4.2`.

### D-62 (H) A third field class: commercial-frozen, for the two non-amendable axes *(autonomous)*

**Decision**: the field classifier gains a **commercial-frozen** class holding
`resourceTenantId` and `sellerTenantId`. An amendment delta naming either is refused with the new
`tenant-axis-immutable` reason. `payerTenantId` stays commercial and amendable **within one
seller's scope**; a payer change that would cross seller scope is **refused** with
`payer-rebinding-requires-seller`, not paired with a seller rebinding.

**Corrected 2026-09-10.** This entry originally said `payerTenantId` was "paired with a seller
rebinding where the change crosses seller scope" — which the same decision makes impossible, since
freezing `sellerTenantId` means no amendment can carry the paired half. The register entry was
itself the source of the contradiction `04 §2.2` inherited, so the pairing language is removed
rather than reworded: there is no post-submit path that rebinds a seller, so the cross-seller payer
change has no admissible form and is refused.

**This diverges from a PRD MUST, and the divergence is now routed rather than decided here.** The
pairing language did not originate in this register — PRD §6.1 requires that a cross-seller payer
change "**MUST** follow the paired payer/seller rebinding semantics", and §12's acceptance
criterion restates it. Removing the pairing therefore does not resolve the contradiction, it
relocates it: this design refuses an operation the PRD requires be honoured. The refusal stands as
specified, because the alternative reachable from here is an unguarded amendment that can rebind
the selling party. The reconciliation — amend §6.1, specify an ownership-transfer transition that
moves both axes together, or accept the refusal — is **Q-28**.

**Rationale**: PRD §6.1 fixes all three axes at `submitted` and permits exactly one post-submit
mutation. `04 §2.2` asserted that payer was the only axis with an amendment path but registered no
guard, and the classifier's binary commercial/administrative split had no way to express
"commercial but not amendable" — so both other axes were classified commercial, the amendment path
accepted them, and step 4's payer-pairing check did not fire for a delta that changed
`sellerTenantId` alone. A submitted order's resource recipient or selling party could be silently
rebound, which is the mis-billing and seller-attribution failure the PRD locks the axes to prevent.

**Propagated**: `02 §4.3`; `04 §3.3`, `§3.6` *Append Amendment* step 1's commercial-frozen guard
(declared alongside the payer-pairing guard the new class now lets fire correctly), `§4.1`.

### D-63 (H) Two permission-matrix corrections against PRD §6.6 *(autonomous)*

**Decision**: `amend` is separated from the authoring row and granted to Partner Admin only;
Orders Workflow is granted `hold` and `resume` under the same service principal as the other seam
operations.

**Rationale**: PRD §6.6 grants Direct Customer create, submit and cancel — not amend — and the
collapsed authoring row marked them permitted across all four operations, widening a privilege the
PRD withheld and handing self-service callers a path that re-runs the gate and re-pins. Separately
PRD §6.6, §12 AC-15, §5.1 and §6.3 all name Orders Workflow as a hold actor, the sibling gear's
PRD commits to calling hold/resume, and `07 §3.6` already listed Workflow as an actor of the
sequence — while the matrix, which `08 §4.3` makes exhaustive and startup-enforced, denied it.
A `MUST`-level acceptance criterion was therefore unbuildable, and the sibling gear's remediation
path for a permanently failed line had no callable operation.

**Propagated**: `08 §4.3`.

### D-64 (H) A draft carries version 1; submit appends version 2 *(autonomous)*

**Decision**: creation appends version 1 — an empty commercial document — so
`orders_order.current_version` is **NOT NULL** from creation. Draft content lives in the mutable
working tables and submit materialises it into **version 2**. Draft mutation and the
administrative edit are state-only rows that present the current version as their expected
version like every other transition.

**Rationale**: `01 §3.7` declared `current_version` "nullable until first version" while the same
section said the aggregate row and its first version are inserted in one transaction, and PRD §12
AC-1 requires the version to be 1 on draft creation — three statements, no two compatible. Worse,
the optimistic version check is mandatory on every transition, so a null `current_version` through
the whole draft phase meant draft operations either bypassed the check (a hole stated nowhere) or
refused against null.

**Propagated**: `01 §3.7`, `§4.1`; `02 §4.1`.

### D-65 (H) Authorization precedes the idempotency probe; the probe precedes guard inputs

**Decision**: the transition algorithm evaluates authorization before it probes the registry for
`(operation, key)`. An unauthorized caller is audited and refused without probing or returning an
idempotency outcome. For an authorized caller, the advisory probe occurs before any guard input is
resolved and returns a fingerprint-matching settled outcome immediately; authoritative resolution
remains inside the transaction.

**Rationale**: An idempotency outcome is order state. Returning one before authorization let any
caller holding a matching scoped key — `(operation, principal_scope, idempotency_key)` since D-88 — and request fingerprint learn a committed
outcome, contradicting the engine's confidentiality rule. After authorization, PRD §12 AC-4 still
requires a replay to "return the same result without creating a second order **or re-running the
sellability gate**". Without the advisory probe, every authorized retry of a committed submit
re-invokes **five** submit-path upstream ports under the full 1.5 s submit budget — and could then meet a deadline or an
adopted-predicate refusal that the engine discards in favour of the stored success, making the
upstream load pure waste and the observability series misleading.

**Propagated**: `01 §2.1`, `§3.6` step 1, `§4.1`, `§4.2`.

### D-66 (H) Both begin-fulfillment elections are policy rows with safe fallbacks *(autonomous)*

**Decision**: `orders_policy_election` (introduced by `05 §3.7`) holds
`tolerate_authorization_failure` and `acceptance_required` keyed `(election, scope, scope_id)`,
seller scope overriding platform. An unset election reads as its safe value — tolerate-failure not
elected, acceptance required — and the guard records whether it read a row or the fallback.

**Rationale**: both guards were specified as reads with no source: no table, no configuration key,
no scope, no default and no delivery path, and neither appeared in the policy-value registers of
`07 §4.5` or `08 §4.5`. Unlike the TTLs, where "no code default" is a deliberate and visible
failure mode backed by a policy table, these had nowhere to be set at all — so an implementer
would have invented a default for the two guards that decide whether a non-paying tenant gets
resources and whether fulfilment may start without recorded consent. `05 §2.1` forbids exactly
that for acceptance.

**Propagated**: `05 §3.7`, `§4.3`; `DESIGN.md §3.7`.

### D-67 (M) Every event carries a common order-summary block *(autonomous)*

**Decision**: `01 §4.4` declares one common summary block — `orderId`, `orderVersion`,
`category`, resulting `state`, the three tenant axes, the contract reference and the external
reference where present — carried by **every** event; the per-event table lists only what each
event adds beyond the envelope and that block.

**Rationale**: PRD §9.2 requires "sufficient order summary fields for consumers to act without a
callback read". The design stated that as a blanket `MUST` but enumerated order content for only
`OrderSubmitted` and `OrderCompleted`; the other nine listed their delta alone, so what satisfied
the requirement for them was unspecified and would have been decided per-implementation. Because
the event contract is a declared stability zone, adding the members later is only non-breaking if
no consumer had already compensated.

**Propagated**: `01 §4.4`.

### D-68 (M) A cross-tenant read is refused as not-found, not forbidden *(autonomous)*

**Decision**: where the caller has no relationship to the order, every read path returns
`order-not-found`. PRD §12 AC-21's confidentiality requirement is met in full; its stated refusal
*kind* — "an authorization error" — is not, and the criterion's wording is the thing that needs
amending. Recorded rather than left for a tester to discover.

**Rationale**: a forbidden response confirms the order exists, turning the read surface into an
enumeration oracle across tenancy boundaries. The anti-enumeration posture is right and the AC's
wording is the weaker constraint, but a blocking show-stopper cannot be signed off against a
substituted outcome that no document acknowledges.

**Propagated**: `08 §4.4`.

### D-69 (M) Adding a state or event type is additive; consumers must tolerate unknown values *(autonomous)*

**Decision**: `01 §4.6` and `DESIGN.md §3.3` classify **adding** a state or an event type as
additive and non-breaking, conditional on a stated consumer obligation: a consumer **MUST**
tolerate an unknown `state` or event-type value and **MUST NOT** exhaustively match either
enumeration. Removal or renaming remains breaking.

**Rationale**: PRD §8's Versatility show-stopper requires the state machine to be extensible
without breaking consumers when new states are added — the only §8 criterion about forward
compatibility rather than a threshold. The breaking-change definition classified removal and
renaming and said nothing about addition, so the criterion had no answer and a future state
addition would have been argued either way with no prior decision.

**Propagated**: `01 §4.6`; `DESIGN.md §3.3`.

### D-70 (M) The audit read is a design-introduced surface with no FR basis *(autonomous)*

**Decision**: `DESIGN.md §3.3` reclassifies the audit read. Ten of the eleven §9.1-absent
endpoints have an FR basis; the audit read does not, and it joins the operator re-drive as a
design-introduced surface needing Product's acknowledgement.

**Rationale**: PRD §6.1 requires every transition to *be recorded* and the audit NFR requires
complete logging — both obligations on writing, not on exposing — and §9.1 contains no
audit-retrieval operation. The slice grounds the surface in a rationale ("a complete audit nobody
can read is not an audit"), which is a reason, not a requirement basis. The distinction between
"described in §6, omitted from §9.1" and "no PRD basis at all" is how this design keeps its scope
extensions honest, and one endpoint was on the wrong side of it — one that exposes actor
identities, delegation-proof references and correlation identifiers.

**Propagated**: `DESIGN.md §3.3`.

### D-71 (H) Acceptance is recordable on the partner path regardless of the required flag *(autonomous)*

**Decision**: a recording attempt on the partner-placed path is admitted whether or not acceptance
is required; `requirement_source` records `volunteered` where policy did not demand it. Only the
self-service path refuses a separate recording, because there the instant rode the submit commit.

**Rationale**: PRD §6.1's "a customer-acceptance instant **MUST** be recordable as a first-class
fact" is unconditional — the required flag appears in the next sentence and governs only whether
fulfilment waits. The design read the flag as governing recordability too, so on an uncontracted
or platform-default partner-placed order a genuine customer agreement could not be recorded at
all. That is the common case, not an edge, and it is exactly the dispute scenario the requirement
exists for. Provenance on the row keeps the evidentiary hygiene the refusal was protecting.

**Propagated**: `05 §3.6` *Record Acceptance* step 3 (the `recording-path-admissible` guard
input), `§4.1`.

### D-72 (H) Fail closed on an unevaluable gate input *(autonomous, now carries ADR-0003)*

**Decision**: an input the submit gate cannot evaluate is a **refusal** with its own reason,
distinct from a predicate that was evaluated and failed. The closed port-reason set is
catalog-predicates-unavailable, identity-party-unavailable, contract-resolution-unavailable,
overlap-presence-unevaluable, evaluation-unavailable and indicative-tax-unavailable. The rejected alternatives were admitting
and relying on the activation-time re-check, and admitting under a seller tolerated-risk election
of the kind `05-preconditions` uses for payment authorization.

**Rationale**: the posture was stated as two constraints in `03 §2.1` and `§2.2` and recorded as a
decision **nowhere** — the 2026-09-09 review found no register entry and no ADR, despite the
posture determining that no submit can pass the gate until `SUB-O5` and three pricing predicate
lanes land. The asymmetry with the payment-authorization election needed recording too: a
tolerating seller there accepts a *credit risk* they own and can price, whereas an unevaluable
sellability predicate would have them accept an *unknown* nobody established. Full alternatives
analysis in [`ADR/0003`](./ADR/0003-cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate.md).

**Propagated**: `03 §2.1`, `§2.2`, `§3.3`; `design/README.md` authoring status.

### D-73 (H) Every stored verdict records its deciding authority *(autonomous)*

**Decision**: a stored approval verdict **MUST** carry a named deciding authority and the order
version it was decided against; a reflection lacking an authority is refused. While the approval
policy owner is unspecified, the stand-in that returns "approval not required" **MUST** be
recorded by name.

**Rationale**: `06 §1.2` declared this decision worth a register entry and none existed, so the
rule lived in slice prose with no propagation address and nothing mechanised could catch it being
dropped. It is the only defence the design has while the policy owner does not exist: given the
stand-in currently exempts every order, distinguishing "policy exempted this" from "nobody ever
asked" is the entire audit value of the field.

**Propagated**: `06 §3.6`, `§4.2`; `01 §3.7` `orders_approval_reflection`.

### D-74 (M) The per-line result is a projection, not a state machine *(autonomous)*

**Decision**: `orders_line_fulfillment` is a **read-only projection** advanced only by the
acknowledgement transition. No per-line state machine, no per-line guards, no per-line events.

**Rationale**: `06 §1.2` declared this worth a register entry and none existed. PRD §6.1 mandates
the absence of a per-line state machine, so this is a recorded constraint rather than a free
choice — but it is load-bearing for R5, which forbids mirroring downstream per-request status, and
it needed a propagation address so a later author does not grow the projection into a machine.

**Propagated**: `06 §4.5`; `01 §3.7` `orders_line_fulfillment`; `08 §3.6`.

## I. Slice-local calls — each declared by its slice as warranting an entry

Six slices named a decision in their `§1.2` as warranting a register entry and none existed, so
each rule lived in slice prose with no propagation address and nothing mechanised could catch it
being dropped. They are determinate calls with a stated alternative, so a table row is the right
shape.

| ID | Sev | Decision | Rationale | Propagated |
|----|-----|----------|-----------|-----------|
| D-75 | [M] | The submit gate reports **every** predicate failure rather than short-circuiting on the first | A caller fixing one refusal at a time needs as many round trips as it has problems, each costing the full port budget; the rejected alternative was short-circuit evaluation, cheaper per call and worse per basket | `03 §3.6` *Run Gate and Submit* step 10, `§4.2` |
| D-76 | [M] | The order-time total **excludes** overlays needing subscription-level evaluation context, and the exclusion is stated on the read and Preview responses rather than left implicit | A total that silently omits an overlay is worse than one that says what it omits; the rejected alternative was computing them from an order-level approximation, which would have this gear deriving price. Interim until `…-upreq-pre-subscription-evaluation` lands; closes PRD §15 row 6 | `03 §4.5`; `08 §4.2`; `UPSTREAM_REQS.md §2.2` |
| D-77 | [M] | An amendment **carries forward** the prior version's commercial content and re-resolves only what the gate produces | The rejected alternative was requiring the caller to resubmit the whole document, which makes every amendment a chance to drop a line by omission and gives the diff no meaning | `04 §3.6` *Append Amendment* steps 3-8, `§4.2` |
| D-78 | [H] | The payment-authorization outcome is consumed as a **guard input** and never stored as an order fact | Storing it would make the order a second record of a payment state it does not own and cannot keep current; the rejected alternative was a twelfth `payment_pending` state, which would need its own TTL, guards, event and table row to represent a condition that is external and transient. Only the *tolerated-failure* decision is stored, because that is a decision this gear's actor took | `05 §4.3`, `§4.4`; `01 §3.7` |
| D-79 | [M] | **See D-74.** No separate decision remains. | Duplicate of D-74; retained only as a stable historical reference. | D-74 |
| D-80 | [M] | The **pre-hold state is stored** on the aggregate rather than derived from the audit trail | The rejected alternative was reconstructing it from the last transition before the hold — a read that derives state from the audit store, which `01 §4.4` forbids outright, and which would break silently if a hold ever followed a non-state-changing transition | `07 §4.1`; `01 §3.7` `orders_order` |
| D-81 | [M] | The read projection **is the aggregate row**, not a separately maintained materialised view | The rejected alternative was an asynchronously updated projection, which would reintroduce the replication lag `08 §4.2` forbids and make the read's freshness a second thing to reason about; the cost is that read shape and write shape are coupled | `08 §4.1`, `§4.2` |

### D-82 (M) The version reason vocabulary is `{create, submit, amendment}`

**Decision**: `orders_order_version.reason` carries exactly `create`, `submit` or `amendment`.
Creation produces empty version 1 as required by PRD §12 AC-1; submit materialises the draft into
version 2. The six reasons PRD §6.2 lists — approval reflection, hold, resume, cancel, fulfillment
outcome, expiry — name state-only transitions and live on `orders_transition_audit.reason`.

**Rationale**: five transition rows append a version: creation, submit and the three amendment
rows. The PRD's eight reasons all describe audit activity, but only `submit` and `amendment`
identify its specified commercial-version causes; `create` is the required additional reason for
the version-1 creation row. The split is correct; the version vocabulary must state the complete
closed set rather than imply the state-only reasons belong on the version chain.

**Propagated**: `01 §3.7`, `§4.3`; `02 §4.1`; `04 §3.3`, `§4.5`.

### D-83 (H) The in-flight order cap stays at one; route (b) does not resolve Q-05 *(carries [`ADR/0007`](./ADR/0007-cpt-cf-bss-orders-lifecycle-adr-in-transaction-concurrency.md))*

**Decision**: gate predicate 9 and `orders_inflight_overlap_claim` enforce **exactly one**
in-flight order per `(payer_tenant_id, overlap_scope_key)`, by a partial UNIQUE index on that pair.
The cap is **not** shared with predicate 7's concurrent-subscription cardinality and is **not**
configurable. Predicate 9 excludes the requesting order from its own count.

**Rationale**: this entry previously decided the opposite — a `slot` column letting the index
express *at most N*, so that raising `maxConcurrentActive` would admit N concurrent in-flight
orders and unblock the partner case of Q-05 by "route (b)". That was wrong on the requirement and
wrong in the algorithm, and both errors are recorded here rather than quietly reverted.

**Wrong on the requirement.** PRD §6.1 states two separate rules. Clause (f) bounds
concurrent-active **subscriptions** per key and says explicitly "default cardinality 1,
**configurable via Catalog/Contract `maxConcurrentActive`**". Clause (g) bounds in-flight
**orders** — "at most one in-flight order per overlap key — a second submit against the same key
while one is in flight **MUST** be rejected" — and carries no configurability clause at all. The
separation is deliberate: how many subscriptions a payer may hold is a commercial policy; how many
orders may be racing toward the same key is a concurrency-correctness rule. Giving them a shared
cardinality overrode a MUST with no amendment and no disclosure.

**Wrong in the algorithm.** The reworded predicate read "the number of in-flight orders holding
the key is below the cardinality resolved for it", dropping the word **other**. An amendment is
issued by an order that is already in-flight and already holds its key, so at cardinality one the
predicate counted the amending order itself and refused every amendment. The self-exclusion is
restored and its load-bearing role is now stated in `03 §4.2` so it is not dropped again.

**What this means for Q-05.** Route (b) — leave the key, raise `maxConcurrentActive` — gives a
partner more concurrent *subscriptions* but still admits only one in-flight *order* per key, so
the reseller buying one product for many customer tenants must place those orders **serially**.
Whether that is acceptable is a Product question, and it is the one Q-05 now asks: take route (a)
and bind a resource dimension into the key, accept serialised ordering under route (b), or amend
§6.1(g). Route (b) alone does not resolve the case it was chosen for.

**Rejected**: keeping the `slot` column against a future §6.1(g) amendment. Under the requirement
as written the column can only ever hold `0`, so it is schema surface with no expressible state,
and the review that produced this reversal criticised exactly that shape.

**Propagated**: `01 §3.7` `orders_inflight_overlap_claim`; `03 §3.6` *Run Gate and Submit* step 15,
`§4.2` predicates 7 and 9.

### D-84 (H) One order line produces one subscription — Q-02 answered no

**Decision**: several order lines **MAY NOT** compose into a single subscription. The 1:1
line-to-subscription mapping holds without exception for this phase, and it is now **enforced**:
`orders_line_fulfillment` carries a partial UNIQUE on `(order_id, subscription_id)` and a completed
acknowledgement is refused with the new `acknowledgement-subscription-duplicated` reason where two
lines report the same identifier.

**Rationale**: Q-02 asked whether one tenant's several products could be a single subscription with
a multi-product entitlement set. The premise had already moved: `subscriptions/docs/PRD.md` defines
a subscription as carrying **effective-dated composition** (`PlanLink`, `AddOn`), so a subscription
is already multi-product and the real question was whether the **order** may map many lines onto one
subscription's component set.

Answering no costs nothing and keeps the boundary the design already drew. The alternative — letting
lines compose — breaks the mapping in five places, one of them the `OrderCompleted` payload, which is
a published contract read by three gears. Nothing in this phase needs composition: `02 §2.2` already
records that **add-on selection is not expressible** here, because add-on rules are authored in the
pricing gear, and PRD §1 defers a line targeting an existing subscription to the later
`category = change` phase. So the two shapes that would want composition are both already out of
scope, and a no confirms a boundary rather than imposing one.

**What the answer changed, and why it is not merely a confirmation.** The 1:1 claim was asserted in
the `subscription_id` **column comment** — "the spawned subscription; 1:1 with the line" — and
enforced nowhere. The primary key gave one row per line, so a line could not map to two
subscriptions; the reverse was unconstrained. A sibling gear composing two lines and reporting one
identifier twice would have been admitted silently, and `OrderCompleted` would have carried a
non-injective mapping. Declining composition therefore required making the invariant real, not just
recording a preference — an invariant nothing enforces is the defect class this register keeps
finding.

**Obligation this places upstream**: Subscriptions **MUST** create one subscription per activation
intent, since Orders now refuses an acknowledgement that reports otherwise. This is not a new ask —
it is the shape `06 §4.3` already assumes — but it is now a refusal rather than an expectation, so
it is stated here as the seam's contract.

**Propagated**: `01 §3.7` `orders_line_fulfillment`; `06 §3.3`, `§3.6` *Acknowledge Fulfillment*
step 1, `§4.4`.

### D-85 (H) The cross-gear contract surface is GTS-typed *(closes the review's GTS findings)*

**Decision**: three surfaces that cross a gear boundary are GTS types under the namespace this gear
now claims, `gts.cf.bss.orders.*` — the **eleven events** (one abstract order-event base derived
from the platform event base, eleven final derived types, `payload` as the extension field,
`type_uuid` as the outbox discriminator), the **refusal reason registry** (error instances whose
identifiers are the RFC 9457 `type` URIs), and the **order category** (well-known instances rather
than a database enum). The states, the transition table, the guard set and the permission matrix
stay Rust types and configuration. Specified in `01 §4.7`, with the boundary stated in `§4.8`.

**Rationale**: four places named GTS as the domain layer's technology and none of it was
specified — no base type, no extension field, no registry, no identifier ownership, no validation
flow. `guidelines/GTS.md` §14 is a checklist for reviewing a DESIGN, and six of its seven mandatory
items were absent, so the claim was a label.

The cost of the gap was not abstract. The design had **hand-built five artefacts the guideline
supplies**: an `envelope_version` column, a dual-major publication plan for rolling out a breaking
event change across three gears, a bespoke reason registry with flat kebab-case names, a
forward-compatibility obligation pushed onto three consumer teams as prose ("a consumer **MUST**
tolerate an unknown event-type value"), and a `category` enum whose own column comment recorded it
as "open to a third value" while a DB `enum` makes a third value an `ALTER TYPE` plus an OpenAPI
widening plus a coordinated client release. `guidelines/GTS.md` §6.6's exemption for plain strings
requires the set be closed and never grow; `category` failed that on its own text.

Two further consequences decided the events case. A bare `version-conflict` in a log line or a
problem body is **indistinguishable** between this gear and Subscriptions, which both raise it;
prefixed identifiers are not. And the reason set is an explicit open extension point — a slice may
add one with no engine change — so one-name-per-condition was reviewer discipline over a growing
set, and is now a registration constraint.

**Timing is why this is cheap.** The gear has no implementation yet, so changing a published
contract costs nothing beyond the documents. The same change after three consumers had built
against `event_type text` would have been the dual-major rollout the design was already planning
for.

**Rejected**: deleting the four GTS claims and recording the deviation. That was an hour of work
against a day, and defensible only if the guideline were aspirational here. It is not —
`gears/bss/ledger` specifies `x-gts-traits.owns_billing_books` concretely and builds an ADR on it,
and `gears/bss/pricing` ships `gts_id!` in Rust, so a BSS gear declining the platform type system
would be the outlier rather than the norm.

**Propagated**: `01 §1.3`, `§3.3`, `§3.4`, `§3.7` (`orders_order.category`,
`orders_event_outbox.type_uuid`), `§4.7`, `§4.8`, `§4.9`; `DESIGN.md §1.3`, `§3.4`.

### D-86 (H) The overlap collision is taken first, and detected as a row shortfall *(closes a CodeRabbit finding on PR #4775)*

**Decision**: claim maintenance is `01 §3.6` *Attempt Transition* **step 17**, placed **before** the
version append at step 18 and before every other document contribution, and it runs on **every**
row. Sub-step 17.1 releases every claim on a terminal target; 17.2 skips the rows that neither
acquire nor release; 17.3–17.6 are the acquiring path and are **check-then-mutate** — partition the
resolved keys into those the order already holds and those it does not, acquire only the second
group with `ON CONFLICT … DO NOTHING`, refuse on a row shortfall **before releasing anything**, and
release superseded claims only after acquisition succeeds. Four properties are normative with it:
that ordering; keys offered **distinct** (a repeated key inserts one row, and a shortfall count
would otherwise read that as the order colliding with itself); keys offered in a **total order** (so
two concurrent multi-key orders cannot deadlock); and **READ COMMITTED** (under snapshot isolation
the insert raises a serialisation failure instead of reporting a shortfall). The refusal is a
**failed slice guard** under `§4.1`, so the seven-class taxonomy and its four-of-seven settlement
split are unchanged and no eighth class appears.

**Rationale**: `§3.7` asserted the collision was "settled and audited in the same transaction". A
raw unique violation **aborts** the PostgreSQL transaction, and the audit append is step 20 with
the settle at 24 — so the abort would precede both and the promised refusal could not be produced.
`ON CONFLICT … DO NOTHING` is what keeps the transaction alive to write it, and an unmapped
constraint violation surfacing as an infrastructure error is what `§4.2` forbids.

**Ordering, not a savepoint, is the mechanism, and an earlier version of this decision had it
wrong.** That version wrapped the acquisition in a **SAVEPOINT** and rolled back to it on
shortfall. Two things were wrong. The savepoint was unnecessary — with `DO NOTHING` there is no
error to unwind, so it discarded work the refusal path had no reason to discard, including the
prior-version claim release. And it was placed **after** the version append, which meant a refusal
had to unwind a committed version row and a moved current-version pointer in tables that grant no
DELETE; a savepoint rollback would have papered over that, but the phantom version was the real
defect and the savepoint was hiding it. Moving acquisition to step 17 removes both: nothing durable
has been contributed when the decision is taken, so there is nothing to roll back and no nested
transaction boundary anywhere in the algorithm. Consequently
`orders_inflight_overlap_claim` carries **no foreign key** to `orders_order_version` — an FK would
force the version to pre-exist the claim, which is exactly the ordering that produced the phantom.
Enforcement stays where D-26 put it: inside the transaction and inside the index.

**Two further defects came out of the first statement of this decision, and both are corrected
here** *(2026-09-11, CodeRabbit Major on PR #4775 plus one found alongside it)*. The first: that
version released the order's existing claims **before** inserting the replacements. A refusal
commits under `ADR/0005`, so the release committed with it — a refused amendment left its order
non-terminal and **no longer holding its own overlap key**, free for another order to take. The
savepoint this decision removed had been covering exactly that, and the removal turned a hidden
dependency into a live defect; "nothing durable to unwind" was true of the version row and false of
the release. Partitioning fixes it and also removes the self-collision **structurally**, since a key
the order already holds is never re-offered.

The second: claim release on a terminal transition was written as part of the acquisition branch,
whose condition is that the contribution carries resolved overlap keys. **No terminal row carries
any**, so the release never executed and every `completed` order would have held its overlap key
permanently — a leak on the happy path, externally indistinguishable from the deliberate
`in_fulfillment` exemption. It is now sub-step **17.1**, ahead of that branch.

**Propagated**: `01 §3.6` *Attempt Transition* step 17 (sub-steps 17.1–17.6) and steps 18–27,
`§3.7` `orders_inflight_overlap_claim`; `ADR/0007`.

### D-87 (H) A parked outbox row blocks its own order's stream and no other

**Decision**: the drain selects on `delivered_at IS NULL` alone, so a parked dead-letter row stays
visible as a blocked stream head, and it takes only the **contiguous prefix** per `order_id`,
stopping at the first parked or not-yet-due row. No higher sequence for that order publishes while
the parked row is undelivered; other orders are unaffected. The operator re-drive **MUST**
republish the parked row before any later event for that order and **MUST NOT** be used to skip one.

**Rationale**: nothing defined this, and the mechanism was silently working against per-order
ordering: the drain's partial index was `WHERE delivered_at IS NULL AND dead_lettered_at IS NULL`,
which **excluded** parked rows, so sequence *n+1* stayed selectable after *n* had parked. A
consumer would then have received `OrderCompleted` before `OrderSubmitted` — breaking the one
ordering guarantee `(order_id, sequence)` exists to hold, on the path taken precisely when
something has already gone wrong. Skipping was rejected: a consumer cannot reconstruct a
commercial trail from an out-of-order stream, and the alternative cost is bounded — one order's
stream halts, alerted, and closed by re-drive.

**Three mechanism defects introduced with the first statement of this rule, and corrected.** The
rule itself is unchanged; what carried it was not implementable. (1) The stop condition named a
`next_attempt_at` column `orders_event_outbox` did not have, so the backoff had nowhere to live —
the column is now declared, alongside `created_at`, which the type index also referenced without
declaring. (2) The row was placed under **monthly partitioning**, which PostgreSQL forbids to
combine with a unique constraint that omits the partition key — and `(order_id, sequence)` omits
it; the table is therefore **not partitioned**, and the retention sweep rather than the partition
drop bounds it. (3) The drain selected a bounded number of **rows**, which can split one order's
contiguous prefix across two batches; it now selects a bounded number of candidate **`order_id`s**
and takes each one's prefix whole. Purging moved out of the per-shard drain to the retention sweep
for the same reason. Re-drive was also under-specified: it **MUST** set `delivered_at` and clear
`dead_lettered_at` and `next_attempt_at`, and is **refused** while a lower undelivered sequence
exists for that order, so re-drive cannot itself produce the out-of-order publication this rule
prevents. The suspension a parked row imposes on its order's stream is **unbounded** in duration
and disclosed as such.

**Propagated**: `01 §2.2`, `§3.2`, `§3.3`, `§3.6` *Drain Outbox Shard* steps 2, 3 and 4.2.2.4,
`§3.7` `orders_event_outbox`, `§4.4`; `ADR/0006`.

### D-88 (H) The idempotency key is scoped by authorized principal *(closes an IDOR finding)*

**Decision**: `orders_idempotency` gains `principal_scope`, taken from the security context by the
pre-guard and **never** from the request body, and its primary key becomes
`(operation, principal_scope, idempotency_key)`. `order_id` is non-null for every operation except
create, and a settled record whose `order_id` differs from the resolved target refuses as
`idempotency-mismatch` rather than being overwritten. The request fingerprint is defined: a hash
over operation, trigger, resolved target, the three tenant axes, `expected_version` and the
canonicalised contribution — explicitly **not** `correlation_id`, the request instant, headers or
any server-assigned value.

**Rationale**: the key was `(operation, idempotency_key)` over a caller-chosen string, with
`order_id` nullable and outside it, and the fingerprint was defined nowhere in the set. Two
tenants choosing the same human-readable key — `submit-2026-001`, or a client library's sequence
number — collided: at best one received `idempotency-mismatch` on a valid request and was frozen
for the 24-hour window, at worst it resolved another tenant's stored outcome. D-65 solved the
confidentiality half by refusing before the registry is read; it did not address collision.

**Two consequences of the scoping, both stated in `01 §4.2` rather than glossed.** First, scoping
**adds a fifth outcome**: the same key text from a different principal is a different key, so the
request **executes again** where a global key would have de-duplicated it. An earlier statement of
this decision claimed the four outcomes stayed exhaustive; that was wrong, and it matters because
"zero duplicate orders" is therefore a guarantee **per principal** — for every operation but
create the fingerprint's `order_id` and `expected_version` still catch the duplicate, and create
is the one place cross-principal duplication is possible.

Second, the scope **MUST** be a **stable subject identifier**, and `01 §4.2` prohibits deriving it
from session, token, `jti`, delegation-proof, replica or transport identity. Any of those can
differ between a request and its own retry, and a scope that moves makes the retry a different key
— so the retry re-executes and the registry becomes a no-op in precisely the crash-and-retry case
it exists for. A deployment that cannot supply a stable identifier **MUST** fail startup.

**Propagated**: `01 §1.2`, `§3.1`, `§3.2`, `§3.6` *Attempt Transition* steps 1 and 6, `§3.7`
`orders_idempotency`, `§4.2`.

### D-89 (M) The subscription axis of the overlap rule is disclosed as open, not bounded by a timed window

**Decision**: the order axis of the overlap rule is closed in-transaction by
`01 §3.7`'s index; the **subscription** axis **MUST** be closed by Subscriptions re-evaluating
`overlapScopeKey` and committing `active` under one reservation boundary, and this gear **MUST
NOT** present its re-check as that boundary. Until that upstream enforcement exists **the gap is
open and this design does not bound it.** Two obligations remain, and both are expressible through
declared interfaces: the re-check is specified as an **early abort** carrying no admission
guarantee, and a collision appearing at or after activation **MUST** surface as an
`overlap-collision` line rejection on the failure-acknowledgement path with compensation evidence.

**Rationale**: the re-check was a bare presence read, and the in-flight claim bounds *orders*, not
active subscriptions — so two activation waves could both pass it and exceed `maxConcurrentActive`.
Atomicity is unreachable from this gear because the committing transaction belongs to another.

An earlier version of this decision bounded the gap on three terms, the first being a **30-second
verdict validity window**. It is withdrawn, because the window was not implementable from anything
this design declares. No port operation, event payload or endpoint response carries a validity
origin or a deadline, and the transition the caller then drives — `spawn-signal`, `01 §4.3` row 12
— is event-less, so the expiry could not be communicated. "Re-invoke the re-check" placed a
**MUST** on a party this gear cannot signal and whose violation it cannot observe. The design's own
two-phase barrier puts a whole fulfillment wave between the read and the last line's activation, so
one window could not cover N activations. And the origin would be read on one gear's clock and
evaluated on another's, with no declared clock source and no skew bound, so both skew directions
fail silently — while the gate's 10-second breaker hold would consume a third of the window by
itself. A bound nobody can enforce or detect the breach of is worse than a disclosed gap, because it
reads as protection.

The closable form is recorded rather than adopted: a **server-side relative TTL enforced at
`spawn-signal`**, where the engine persists the re-check instant and refuses the spawn signal if it
is older than a configured age — one clock, persisted state, the party that owns the transition. It
needs a column, a guard, a refusal reason and a value, none of which this design set has.

**Propagated**: `03 §2.2`, `§3.6` *Re-check Activation Preconditions* steps 3 and 5, `06 §4.3`,
`UPSTREAM_REQS.md` §2.1 (`SUB-O5` enforcement ask).

### D-90 (H) Bounded lifetime is a per-state TTL plus two re-entry caps, and the residual gap is disclosed

**Decision**: **Layer 1** is the per-state TTL, measured from `state_entered_at`, which a resume
restarts — and which holds only where a TTL is configured. An unconfigured TTL **MUST NOT** block
startup, because the values are Product-owned open questions. **Layer 2** is **two re-entry caps**,
one per transition that resets `state_entered_at`: `orders_order.resume_count`, incremented by
`01 §4.3` row 22, guard `resume-cap-exhausted`, baseline **5**; and
`orders_order.amendment_count`, incremented by rows 18, 19 and 20, guard
`amendment-cap-exhausted`, baseline **20**, owned in `04 §4.1`. No transition resets either.
Together they bound an order at **26 state entries**, so its in-flight life is at most
**26 × the largest configured TTL** among the expirable states. Where a TTL is unset that state is
**unbounded**, and that is disclosed in `07 §4.2` and alerted in `07 §3.8` rather than covered by a
design-owned fallback.

**Rationale**: `state_entered_at` was the sole dwell input and resume rewrites it, so any actor
holding hold permission could cycle hold/resume and keep an order in `submitted` or `approved`
indefinitely — defeating the bounded-lifetime MUST, and with it `05 §4.4`'s declined-instrument
exit, since an order whose payment authorization failed leaves only by expiry. Separately, the
design asserted every in-flight state has a bounded lifetime while tolerating an unset TTL, so the
claim was false wherever the value was missing. The cap closes the first problem at the operation
that creates it. The second is a Product dependency (PRD §15 row 7) and is now stated as one.

**An earlier version of this decision made Layer 2 an absolute order lifetime** measured from
`created_at`, baseline 90 days, enforced by a second sweep pass. It is withdrawn on four grounds,
recorded here because the shape of the mistake is reusable:

* Its enforcement pass shared one deterministic idempotency key with the per-state pass, to avoid double-expiring an order both selected. But refusals settle and replay under their key (ADR-0005), and the key was invariant in the order's version — so once a per-state attempt refused as not-admissible, every absolute-pass request for that order and version **replayed the refusal instead of attempting**. The backstop was inert for exactly the orders something had already gone wrong with, and inert invisibly, since a replayed refusal and a fresh one are the same response.
* It did not close the loop it existed for. The hold that can be cycled indefinitely is the one taken from `in_fulfillment`, which `07 §4.3` exempts from both layers.
* It pre-empted legitimate orders: one gear-level duration cannot distinguish an abandoned order from an enterprise order awaiting a slow approval, and it expired both.
* Its value was a commercial policy with no PRD basis (Q-27), taken autonomously.

The count cap was rejected in that earlier version for permitting `n × TTL` and needing a counter,
a reset rule and a new refusal. That reasoning is inverted here. A finite multiple of a configured
TTL is a **bound**, which is what was asked for; each counter is written by one trigger and reset
by none, so there is no reset rule to get wrong; and the new refusal is the point — a bound whose
breach is a refused, audited transition is observable, where a backstop's failure to fire is not.

**The first statement of this decision capped only resumes, and the loop stayed open**
*(corrected 2026-09-11, from a CodeRabbit Major on PR #4775 and what checking it exposed)*. It
claimed `(cap + 1) × TTL` as the order's bound, which was wrong twice. It bounded **one state's**
repeated dwell, while an order traverses several states each with its own TTL — so it was never an
upper bound on the order. And **amendments reset the dwell too**: rows 19 and 20 target `submitted`
from `pending_approval` and `approved`, the effective target differs from the outgoing state, and
`01 §3.6` *Attempt Transition* step 20.1 sets `state_entered_at`. `approved → submitted → approved`
therefore restarted the clock indefinitely through an entirely uncapped operation — the same defect
on a different trigger.

**Two counters rather than one shared budget**, and the reason is commercial rather than
structural. A single re-entry budget would be tidier — one column, and any future clock-resetting
row covered by construction — but a resume is a **seller-side operational** act and an amendment a
**buyer-side commercial** one, so one budget would let a seller's compliance holds consume a
buyer's ability to correct their own order. The amendment baseline of 20 is argued in `04 §4.1`:
negotiated orders revise two to five times, an amendment is a re-quote rather than an edit, and a
buyer-facing cap must be generous because an order a buyer cannot correct is worse than a
long-lived one.

**Propagated**: `07 §1.1`, `§2.1`, `§2.2`, `§3.1`, `§3.2`, `§3.3`, `§3.6` *Sweep Expired Orders*
and *Hold Then Resume*, `§3.7`, `§3.8`, `§4.1`, `§4.2`, `§4.3`, `§4.4`, `§4.5`, `§5`;
`01 §3.7` `orders_order` schema and index list, `§3.6` steps 20.4 and 21, `§4.3` rows 18, 19, 20
and 22; `04 §3.3`, `§3.6`, `§4.1`; `03 §4.3`; `05 §2.2`; `DESIGN.md` §3.2 and §4.2.

## Open questions

Not decided here. Each carries a named owner and the design position taken in the meantime.

| ID | Question | Owner | Interim position |
|----|----------|-------|------------------|
| Q-01 | PRD §15: should buyer-decided trial conversion and re-negotiated renewal be reclassified as commercially initiated, and if so is it a third `category` or a widening of `change`? | Product with Architecture | `category` remains `new_sale` \| `change` and the enum is documented as **open to a third value**, so the schema no longer forecloses it |
| Q-02 | **Closed — answered no by design decision D-84.** Several order lines may not compose into one subscription; the 1:1 mapping holds and is now enforced by a partial UNIQUE on `(order_id, subscription_id)` and a distinctness guard on acknowledgement. The premise had already moved — a subscription is *already* multi-product via `PlanLink` and `AddOn` — so the question was about the order-side mapping, and both shapes that would want composition (add-on selection, a line targeting an existing subscription) are already out of scope for this phase | Design | Closed; no external decision required |
| Q-03 | PRD §15: does the order line gain commercial soft and hard bounds and an overuse price reference, and what are `qty` semantics per charge kind? | Product with Architecture and Rating | No commercial cap on the line; the PRD's own note that routing it to the quota subsystem places a negotiated term outside the order is now reproduced rather than suppressed |
| Q-04 | The `SUB-O*` register has forked between the Subscriptions seam map (`O1`–`O6`) and the Workflow PRD (`O1`, `O5`–`O9`), with `O6` carrying two meanings. Which numbering is canonical? | Architecture | This gear cites the seam-map numbering and adds `SUB-O10` (D-56); `UPSTREAM_REQS.md` records both readings |
| Q-05 | The default `overlapScopeKey` — `(payerTenantId, catalogSubscriptionProductKey)` at cardinality one — refuses a partner buying the same product for a second customer tenant. **Two upstream routes exist, and this register had recorded only the first**: (a) bind a resource dimension into the key, or (b) leave the key and raise `maxConcurrentActive`, which `subscriptions/docs/SEAMS.md` **SUB-G1** already says "may come from Catalog **or** Contract". Route (b) needs no key change at all. The key's shape is registry-owned by the `products` gear and SUB-G1 names the venue: **engage on PR #4177 before merge** | Architecture with Subscriptions | **Route (b) alone does not resolve this** (D-83): PRD §6.1(g) caps in-flight orders at one with no configurability clause, separately from §6.1(f)'s configurable subscription cardinality, so raising `maxConcurrentActive` lets the partner hold more concurrent subscriptions while still placing their orders **serially**. The choice is therefore route (a), accepting serialised ordering, or amending §6.1(g). The key is applied as adopted and this design must not fork the default locally. Subscriptions' **SUB-O5** already records the same collision with the same compensation-path reasoning, so the problem is understood on both sides of the seam — what is unagreed is only which of the two routes is taken |
| Q-06 | Per-state TTL defaults and whether seller scope may override platform scope | Product | No code default; an unconfigured state is not swept |
| Q-07 | **PRD §15 row 5 carries two values and this row tracks both.** (a) The program retention period for completed and cancelled orders. (b) The **`draft` auto-void TTL**, which `07 §4.5` routes here and which is the more urgent half: while it is unset the draft sweep does no work, basket accumulation is **unbounded**, and there is **no fallback** — the absolute-lifetime backstop that previously supplied one was withdrawn by D-90. `draft` therefore has the largest exposure of any state to an unanswered value, and unlike the §15 row-7 TTLs it bounds storage rather than a commercial promise | Product | (a) Append-only with no destructive path; retention deferred to the policy. (b) No code default, per `07 §2.2` — a default would become the platform answer. The condition is visible on the draft-age distribution (`02 §3.8`) and the no-configured-TTL alert (`07 §3.8`) |
| Q-08 | The minimum payment-outcome surface, given that a declined instrument currently exits only by expiry | Architecture with Product | Authorization-only, stated as a limitation in `05 §4.4` |
| Q-09 | Is the Generic Approval service specified by its own PRD, or by a transitional module-local pattern? | Architecture | Stand-in behind the expectations contract; every verdict carries its deciding authority (D-73) |
| Q-10 | The durable-execution engine for the sibling Workflow gear | Architecture | Out of scope for this gear; nothing in the engine depends on it |
| Q-11 | Nothing bounds **end-to-end submit latency**. The PRD's `p95 < 1 s` is scoped to the commit — "durable write + event publish" (§7.1, §12) — while `03 §2.2` budgets **1.5 s** for the submit-path port resolution that precedes it (1.75 s on Preview, which additionally calls the tax port), so a caller experiences roughly 1.5 s plus the commit and no requirement covers that figure | Product with Architecture | The two budgets are declared separately and measured separately, and `03 §1.2` requires the load test to attribute them; the design does **not** claim the buyer surface is within 1 s |
| Q-12 | PRD §6.1 says amendments from `submitted` and `pending_approval` **do not change order state**, yet D-61 transitions a `pending_approval` amendment to `submitted`; its diagram also declares a direct `approved → pending_approval` edge while this design reaches that state in two steps because the direct edge's guard is unobtainable. Separately §10 UC-002 step 3 and §12 AC-5 require an amendment from `pending_approval` to return to its pre-approval state, while §5.1 and §6.2 scope that clause to `approved` only | Product with Architecture | The two-step shape and the `pending_approval → submitted` divergence are disclosed in `01 §4.3` and `04 §4.3`; the PRD state rule, diagram and §12 AC-5 need reconciling, or a verdict port must be specified and AC-11a relaxed |
| Q-13 | **Closed — no Product decision required; the PRD's own usage settles it.** §12 requires rejecting a superseded version "with a machine-readable **stale-version** reason", and D-59 read that as a descriptor rather than a minted identifier, resolving it to the engine's `version-conflict`. That reading is not an interpretation among several: §12 uses the **identical construction** four more times — "a machine-readable **business-level** reason code", "a machine-readable **business** reason", "a machine-readable **business-level** reason indicating the mixed-currency basket", "a machine-readable **business-level** reason indicating fulfillment has already spawned a subscription" — and in none of those four is the adjective phrase a reason name. A reading that mints `stale-version` would have to mint `business-level` too. So the phrase names the *kind* of reason, `version-conflict` is a reason of that kind, and the `MUST` is met | Design | `01 §4.2` holds the descriptor-to-identifier mapping so a later reader does not restore the descriptor as a name and reintroduce the duplication D-38 removed. Product is **notified, not asked**: if §12 is ever intended to mint identifiers it should say so for all five phrases, which would be a PRD change and not a design one |
| Q-14 | **Closed — no Product decision required.** PRD §6.1 gives the contract-effective date's default as **submit time**. The design now retains an explicitly authored date in draft and, where it remains absent, resolves it at submit from the commit instant before deriving dependent dates | Design | `02 §4.2` now implements the PRD rule; the resolved values are stored with the admitted version and no authoring-time/default divergence remains |
| Q-15 | ADR-0003's fail-closed posture means **no submit passes the gate** until `SUB-O5` and three adopted pricing predicate lanes exist, so operators will see submits refused for predicates not yet built upstream. That behaviour is designed, but it is product-visible and nobody outside this design has agreed it | Product with Architecture | Fail closed is implemented and now recorded as D-72 with ADR-0003; the phase map in `design/README.md` states the consequence |
| Q-16 | The PRD's `p95 < 1 s` reads "durable write **+ event publish**" (§7.1, §12 AC-17), but an outbox makes publication asynchronous by construction and D-41 gives delivery its own **30 s p95** budget. The combined threshold cannot hold as written | Product with Architecture | The split is implemented and now disclosed in `DESIGN.md §1.2` and `§4.1`; either §7.1 and AC-17 are amended to separate commit from delivery, or the drain is resized to a sub-second budget |
| Q-17 | PRD §16's pin-staleness risk asks for an "acceptable staleness window" to be documented in the NFR workshop. The design carries the other two mitigations (re-pin on amendment, downstream seal) and no window; the de facto bound is the per-state TTL, itself unset under Q-06 | Architecture with Product (NFR workshop) | `03 §4.3` now names the TTL as the bound rather than asserting staleness is "bounded" |
| Q-18 | PRD §11's Order Console step 4 lists **hold** among a Partner Admin's actions, while §5.1 and §6.3 name only the seller operator and Orders Workflow as hold actors. This design follows the stricter reading and denies Partner Admin hold | Product | Recorded in `08 §4.3`; either §11's step list or the §5.1/§6.3 actor set needs correcting |
| Q-19 | The **operator outbox re-drive** endpoint has **no PRD basis at all** (D-58) — it is a design-introduced operational surface, and D-58 says it needs Product's acknowledgement. Nothing routed it | Product | Implemented and disclosed in `DESIGN.md §3.3`; acknowledge the surface, or remove it and accept that a parked dead-letter row is unrecoverable |
| Q-20 | The **audit read** has no FR basis (D-70): PRD §6.1 and the audit NFR oblige the system to *record*, not to *expose*, and §9.1 contains no retrieval operation. It exposes actor identities, delegation-proof references and correlation identifiers | Product | Implemented and disclosed in `DESIGN.md §3.3`; acknowledge it, or scope what the surface may return |
| Q-21 | **Preview persists a gate-outcome row per predicate per line** (D-52) against PRD §9.1's specification of Preview as creating and mutating **no state** | Product | Implemented with a 7-day retention and a rate limit (`03 §4.6`); amend §9.1's wording, or drop the persistence and lose the Preview diagnostics |
| Q-22 | Row 6, **`draft → expired`** on the auto-void TTL, is an edge PRD §6.1's normative state diagram does not contain; §7.1 says only that abandoned drafts *SHOULD* be auto-voided (D-14). `OrderExpired` consequently carries two commercially different facts | Product with Architecture | Implemented and disclosed in `01 §4.3`; amend the §6.1 diagram, or introduce a twelfth state and a twelfth event, which ADR-0004 rejected |
| Q-23 | **Ten endpoints the PRD describes in §6 but omits from §9.1** mean §9.1 is no longer the normative operation set (`DESIGN.md §3.3`). Each has an FR basis, so this is a wording gap rather than a scope extension | Product | The endpoints are implemented and inventoried; §9.1 needs the amendment `DESIGN.md §3.3` already calls for |
| Q-24 | The **version reason vocabulary** is `{create, submit, amendment}` (D-82) against PRD §6.2's MUST-level eight-value list; the other six name state-only transitions and live on `orders_transition_audit.reason` | Product | The split is implemented and stated in `04 §4.5`; amend §6.2 so its enumeration matches the version/audit split PRD §1.4 already draws, or a conformance run against §6.2 fails on six values |
| Q-25 | **`OrderAmended`'s PRD trigger no longer holds.** PRD §6.5 emits it "on creation of a new order version", and D-64 makes creation and submit version-appending rows that publish no `OrderAmended` — creation is event-less, submit publishes `OrderSubmitted`. It fires only on the three amendment rows | Product | Implemented and disclosed in `01 §4.4` and `04 §3.3`; amend §6.5 and §9.2 to name amendment as the trigger, or a conformance run against §6.5 fails on two of the five version-appending rows |
| Q-26 | The **operational limits this design set as working baselines** need ratifying against real capacity: the refusal rate limit (20/min per caller-order, 200/min per caller), the per-port bulkhead (32 in-flight), the breaker ratio (0.5 over 30 s, open 10 s), submit and Preview rate limits (10/min and 60/min per caller), the line cap (200) and the outbox bucket count (64). Each was previously named as a mechanism with no value, so five separate risk mitigations rested on thresholds nobody had set | Architecture | Values are set in `01 §3.7`, `02 §4.5`, `03 §2.2` and measured by the load tests those sections name; they are baselines to revise, not guesses to keep |
| Q-27 | **An order in a state with no configured TTL never expires.** PRD §6.3 requires bounded lifetime; PRD §15 row 7 leaves the TTL values open; and this design takes no code default, so the requirement is unmet for exactly the states Product has not yet valued — including `draft`, whose auto-void TTL is Q-07. D-90 records why the absolute-lifetime backstop that previously masked this was withdrawn. This is therefore a **requirement blocked on an unanswered question**, not a design gap | Product | Disclosed in `07 §4.2`, `§4.4` and `§4.5`, alerted per `07 §3.8`, and bounded on the one axis this design can close — the resume cap of D-90 stops the dwell being restarted without limit. Answering §15 row 7 and Q-07 closes it; no mechanism changes when they are answered |
| Q-28 | **D-62 refuses a cross-seller payer rebinding that PRD §6.1 requires be honoured.** The PRD says a payer change crossing seller scope "**MUST** follow the paired payer/seller rebinding semantics (ownership-transfer alignment, manifest §4.11)", and §12's acceptance criterion repeats it — "paired with seller rebinding where the change crosses seller scope". D-62 freezes `sellerTenantId` as commercial-frozen, which makes the paired half unexpressible, and refuses the cross-seller payer change with `payer-rebinding-requires-seller`. The divergence is deliberate and was taken to close a real hole (an unguarded amendment could rebind the selling party), but it narrows a PRD MUST and the register recorded it as a decision rather than routing it | Product + Architecture | The freeze and the refusal are implemented as D-62 states (`04 §2.2`, `§3.3`, `§3.6`); a cross-seller payer change is therefore **not supported** and a caller must cancel and re-place. Closing it needs one of three: amend §6.1 to match, specify an ownership-transfer transition that rebinds both axes together under its own guard and event, or accept the refusal as the answer. Nothing changes in this design until it is answered |
| Q-29 | **PRD §1.1 claims the order does "double duty as quote and order" with "validity/expiry [as] the per-state TTL", and no state in this design is a quote.** A commercial quote is a *priced, non-binding, time-bounded offer*. A `draft` carries no price (`02 §3.2`); submit is where the price appears and on the self-service path submit **is** the commitment (`05 §4.2`); Preview prices a basket but persists only its per-predicate verdicts, so the figure it quoted is unrecoverable and bound for no period (`03 §4.6`). A partner-led sale needing "valid for thirty days" must hold that price outside this SoR with its validity unenforced — the outcome §1.1 gives as the reason no separate quote artifact is needed | Product + Architecture | Disclosed in `03 §4.6`. This design **MUST NOT** close it locally by storing Preview's total and calling it an offer: an offer needs a validity rule, an expiry actor, a re-price rule and a binding-on-acceptance rule, none of which any document in this set carries. Closing it means amending §1.1 to stop claiming quote coverage, or specifying a priced offer artifact — a scope decision, not a design one |
| Q-30 | **The partner path cannot complete without a `resourceTenantId` principal who can reach an acceptance surface.** `05 §4.2` bars the placing and selling parties from recording acceptance — the only technical control against manufactured consent, and kept. But where the end customer has no platform credential at the point of sale, nobody may record it: begin-fulfillment refuses, the order rests in `approved`, and with the TTL unset (Q-06, Q-27) it never leaves. A partner-placed order can be commercially agreed offline and still be unfulfillable | Product + whoever owns partner onboarding | Disclosed in `05 §4.2`. The platform **MUST** be able to present an acceptance action to a `resourceTenantId` principal for any order the partner path produces — a capability this gear does not own. No delegated or operator-attested route is offered, deliberately, since an attested route is the authority artifact D-31 found unspecified. The available mitigation is a seller electing acceptance **not** required (`05 §4.1`) — a decision about evidence, to be made knowingly |

## Traceability

- **PRD**: [`./PRD.md`](./PRD.md)
- **DESIGN**: [`./DESIGN.md`](./DESIGN.md) and [`./design/`](./design/)
- **ADRs**: [`./ADR/`](./ADR/) — `cpt-cf-bss-orders-lifecycle-adr-transition-through-engine`, `cpt-cf-bss-orders-lifecycle-adr-slice-decomposition`, `cpt-cf-bss-orders-lifecycle-adr-fail-closed-gate`, `cpt-cf-bss-orders-lifecycle-adr-closed-enumerations`, `cpt-cf-bss-orders-lifecycle-adr-refusals-commit`
- **Review waves**: the 2026-09-08 wave (`R-01`…`R-74`) and the 2026-09-09/10 waves (`Rc2-`, `Rc3-`, `F2-`, `F3-` ids); records held with the team rather than in this set
