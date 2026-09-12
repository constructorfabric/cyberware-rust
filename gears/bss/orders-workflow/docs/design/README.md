<!-- CONFLUENCE_TITLE: [BSS]: Orders Workflow — Design Set -->
<!-- Related: ../DESIGN.md, ../PRD.md | Owners: BSS Orders team -->

# Orders Workflow — Design Set

<!-- toc -->

- [Slice documents](#slice-documents)
- [Slice map (PRD ↔ implementation phase)](#slice-map-prd--implementation-phase)
- [Honesty disclosures](#honesty-disclosures)
- [Validation posture](#validation-posture)

<!-- /toc -->

This folder holds the Orders Workflow technical design as a **set of slice designs**: a shared
**process engine** (`01-foundation.md`) plus per-capability handler designs. Every slice runs on
top of the Engine — the process-instance aggregate, definition-version pinning, the durable step
log, the retry budget (per-attempt timeout and step deadline as distinct bounds), the dead-letter
path, concurrency and back-pressure control, the process audit log, the event outbox, and the
machine-readable reason catalogue. The Engine owns no fulfillment or approval policy; each slice
is a handler that runs steps against the Engine's process API.

This gear acts **on** the order document but does not own it — Orders Lifecycle is the system of
record. All provisioning flows through Subscriptions; this gear never invokes OSS directly.

**[`../DESIGN.md`](../DESIGN.md) is the canonical index for the architecture overview, the
component model, the cross-cutting posture, and a registry of the gear's tables — which table is
specified where, who owns its content, and whether it is mutable.** It is an index, not a second
schema: column-level schemas, endpoint paths and state enums live in the slices and nowhere else,
so there is exactly one normative statement of each and no "authoritative once authored" deferral
between them. Requirements (WHAT/WHY) live in
[`../PRD.md`](../PRD.md); decision rationale lives in [`../ADR/`](../ADR/) and in the register
[`../DECISIONS.md`](../DECISIONS.md); asks on gears this one does not own live in
[`../UPSTREAM_REQS.md`](../UPSTREAM_REQS.md).

## Slice documents

- [`01-foundation.md`](./01-foundation.md) — **shared process engine**: process-instance
  aggregate, definition-version pinning, durable step log, retry budget with per-attempt timeout
  and step deadline as distinct bounds, dead-letter handling, concurrency and back-pressure
  control, process audit log, event outbox, machine-readable reason catalogue.
- [`02-triggers-and-start.md`](./02-triggers-and-start.md) — process start: the events and
  conditions that spawn a process instance, definition selection and version pinning at start,
  idempotent start on duplicate triggers.
- [`03-approval-execution.md`](./03-approval-execution.md) — the approval step: the expectations
  contract the workflow holds against an approval decision, the stand-in behaviour while the
  Generic Approval service does not exist, and how a returned verdict resumes the process.
- [`04-fulfillment-plan.md`](./04-fulfillment-plan.md) — building the per-line fulfillment plan
  from the pinned order, the pre-activation abort check, and the two-wave activation barrier.
- [`05-provisioning-intents.md`](./05-provisioning-intents.md) — the provisioning-intent record
  sent to Subscriptions per line, its lifecycle, and the linkage back to the fulfillment plan.
- [`06-saga-and-compensation.md`](./06-saga-and-compensation.md) — the saga over provisioning
  intents, the compensable-no-pivot rule, and the compensation log as designed-time-declared
  reversal steps.
- [`07-manual-tasks.md`](./07-manual-tasks.md) — the manual-task record raised on unresolved
  partial failure, its queue, and its resolution paths back into the process or to dead-letter.
- [`08-hold-and-cancel.md`](./08-hold-and-cancel.md) — operator/caller-initiated hold and cancel
  of an in-flight process instance, including cancel with compensation evidence.
- [`09-read-and-authz.md`](./09-read-and-authz.md) — process-instance and task read projections,
  and the per-actor authorization matrix over them.

## Slice map (PRD ↔ implementation phase)

The word "phase" carries three different meanings across this design set, so this table fixes
which one it uses: **the `Phase` column below is the build phase — the order in which the slices
are implemented.** It is not the *delivery* phase a slice body means when it says a capability is
"inert in phase 1" (that is about what ships working, and is driven by whether an upstream
dependency exists), and it is not the authoring-plan phase number some slices cite when pointing at
where a section was written. Where either of those appears elsewhere, it refers to a different axis
and does not index this table.

The numeric prefix is **implementation build order**, not the PRD section number — the two axes
deliberately do not line up. A slice is scoped by PRD decomposition but built when its
dependencies exist. [`../ADR/0002`](../ADR/0002-cpt-cf-bss-orders-workflow-adr-slice-decomposition.md)
(`cpt-cf-bss-orders-workflow-adr-slice-decomposition`) names this table the build-order authority.

| Doc | PRD § | Phase | Depends on |
|-----|-------|-------|------------|
| `01-foundation` | process engine core | 0/1 | — |
| `02-triggers-and-start` | trigger + start | 1 | 01 |
| `03-approval-execution` | approval | 1 | 01, 02 |
| `04-fulfillment-plan` | fulfillment planning | 1/2 | 01, 02, 03 |
| `05-provisioning-intents` | provisioning | 2 | 01, 04 |
| `06-saga-and-compensation` | saga / compensation | 2 | 01, 05 |
| `07-manual-tasks` | manual tasks | 2/3 | 01, 05, 06 |
| `08-hold-and-cancel` | hold / cancel | 3 | 01, 02, 03, 05, 06 |
| `09-read-and-authz` | reads / authz | 3/4 | 01–08 |

Several of these edges are less obvious than the rest and are stated because they were checked
against the component and table ownership established in `DESIGN.md` §3. `03` needs `02` because
the approval step only runs inside a started, version-pinned process instance. `04` needs `03`
because the fulfillment plan is only built once the process has passed (or been made to bypass)
the approval gate. `05` needs `04` because a provisioning intent is derived line-by-line from the
fulfillment plan, not from the raw order. `06` needs `05` because the saga compensates
provisioning intents, so the intents must exist before their reversal can be designed. `07` needs
`05` and `06` because a manual task is raised on unresolved partial failure inside the
provisioning/saga path, and its resolution re-enters that same path. `08` needs `02`, `03`, `05`
and `06` because hold and cancel must be able to interrupt the process at start, at the approval
step, and mid-saga, and cancel must record compensation evidence produced by `06`. `09` depends
on `01` through `08` because its read projections and authorization matrix cover every state and
actor surface the other eight slices introduce; nothing here is separable from the whole set.

## Honesty disclosures

Two gaps are stated here plainly rather than left implicit in the slice documents.

**(a) The phase-1 approval path is inert.** Until the Generic Approval service exists, a stand-in
sits behind the same expectations contract and unconditionally returns "approval not required",
and that stand-in must be recorded as the deciding authority by name on every verdict it produces.
As a direct consequence, multi-party approval gates, escalation timers, and the approver inbox
never fire in this phase, and two of the six process events —
`OrderApprovalRequested` and `OrderApprovalEscalated` — never fire either. The Generic Approval
service has **no specification anywhere in this repository**; `03-approval-execution.md` designs
against its expectations contract only, not against an implementation.

**(b) Fulfillment's pre-activation abort check cannot be fully evaluated today.** One half of that
check needs the upstream overlap-presence read `SUB-O5`. `SUB-O5` is registered on the upstream
seam map but is **unagreed** — until it lands, that half of the check is unevaluable, and
`04-fulfillment-plan.md` must fail closed on it rather than assume a result.

## Validation posture

The nine slice documents are **deliberately outside `cfs` autodetect and traceability**, per
`.cf-studio/config/artifacts.toml`. `cfs` hardcodes its artifact-kind set, and a `DESIGN_SLICE`
kind cannot be registered — this was attempted and reverted on 2026-07-28. The slices follow the
DESIGN template and declare `cpt-cf-bss-orders-workflow-*` ids, but they are **review-audited**
rather than `cfs`-validated, the same posture already used by the pricing, rating and
subscriptions design sets. Concretely: this means the set's coherence claims are
**reviewer-checkable statements, not CI-enforced ones**. Those claims are: that the dependency
edges in the slice map line up with the component and table ownership the slices declare; that each
`cpt-cf-bss-orders-workflow-*` id is minted in exactly one document and referenced everywhere else;
that every table a slice specifies appears as a row in `DESIGN.md`'s table registry naming that
slice as its specifying document, and that no table's columns, endpoints or state enums are
declared in both places; and that the process-event set the slices declare is the same set
`DESIGN.md` names. Nothing above is enforced — each is a statement a reviewer can check and, until
one does, an assertion this set makes about itself. `design/README.md` itself is likewise
excluded from `cfs` autodetect, so `cfs validate --artifact` reports it unmatched; that is
expected, not an error.
