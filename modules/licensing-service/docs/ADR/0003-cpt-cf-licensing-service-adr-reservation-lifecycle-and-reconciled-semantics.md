---
status: proposed
date: 2026-04-20
decision_owner: Platform Engineering
approver: Architecture Review Board
scope: licensing-service; all ModKit modules integrating quota reservation and reconciliation
---

# ADR-0003: Reservation Lifecycle Closed Set and `reconciled` Audit Semantics

**ID**: `cpt-cf-licensing-service-adr-reservation-lifecycle-and-reconciled-semantics`

- **Priority applicability**: p2/p3

## Context and Problem Statement

The Licensing Service PRD §5.7–§5.9 specifies the reservation lifecycle used by the quota layer. Multiple
requirements refer to lifecycle states, but the authoritative closed set of states was not stated consistently across
the document:

- `cpt-cf-licensing-service-fr-reservation-lifecycle` enumerated four states: `reserved`, `committed`, `released`,
  `expired`.
- `cpt-cf-licensing-service-fr-durable-usage-accounting`, `cpt-cf-licensing-service-fr-usage-summary`, and
  `cpt-cf-licensing-service-fr-shared-recovery-state-machine` enumerated five: the four above plus `reconciled`.

Without a single canonical closed set, independent implementations could legitimately interpret `reconciled` either
as a fifth lifecycle state with in/out transitions, or as a post-terminal audit attribute on records that are
already `committed`, `released`, or `expired`. Each interpretation leads to different state-machine rules, different
aggregation math in usage summaries, and different reconciliation-worker responsibilities.

This ambiguity is correctness-relevant: it affects whether `reconciled` records can be summed alongside the four
lifecycle-state buckets, whether reconciliation alters terminal state, and whether the state machine accepts or
rejects transitions into a `reconciled` state.

## Decision Drivers

- Exactly one canonical closed set for the reservation lifecycle, consistent across every requirement that mentions it
- Minimal change to the existing state machine already described in `fr-reservation-lifecycle`
- Clear separation between state transitions (owned by consumer modules and the reservation API) and post-terminal
  audit operations (owned by the reconciliation worker)
- Aggregation safety for `fr-usage-summary`: no hidden double-counting of the same physical usage across lifecycle
  and audit dimensions
- Compatibility with `fr-shared-recovery-state-machine`, which routes reconciliation through the same SDK state
  machine as lifecycle transitions

## Considered Options

- **Option A — `reconciled` as a post-terminal audit flag.** The reservation lifecycle is a four-state closed set
  (`reserved`, `committed`, `released`, `expired`). `reconciled` is a separate boolean audit attribute persisted on
  an already-terminal record by the reconciliation worker. Setting `reconciled` does not change the record's
  lifecycle state and does not release or re-allocate quota capacity. Usage summaries continue to surface
  `reconciled` counts, but as a view over already-terminal records, not as a fifth disjoint bucket.
- **Option B — `reconciled` as a fifth terminal lifecycle state.** The reservation lifecycle becomes a five-state
  closed set. The transitions `committed → reconciled` and `expired → reconciled` are permitted; `reconciled` is
  absorbing and terminal; `released` does not participate in reconciliation. Aggregate quantities across the five
  states are disjoint by construction.
- **Option C — Remove `reconciled` from the specification.** The reservation lifecycle remains a four-state closed
  set. Reconciliation continues to exist as a process owned by the reconciliation worker, but no per-record flag or
  counter surfaces its progress. Operators infer reconciliation progress from external process telemetry rather than
  from usage-summary fields.

## Decision Outcome

Chosen option: **Option A — `reconciled` is a post-terminal audit flag**.

The Licensing Service PRD will:

This ADR describes the target reservation / reconciliation model that becomes operationally relevant once
`released` lifecycle semantics and reconciliation are in scope (p2) and reaches full audit strictness in p3.

- Declare `reserved`, `committed`, `released`, `expired` as the exact closed set of reservation lifecycle states in
  `cpt-cf-licensing-service-fr-reservation-lifecycle`. `committed`, `released`, and `expired` are terminal.
- Specify that `reconciled` is a boolean audit flag persisted on an already-terminal reservation or usage record by
  the reconciliation worker. Setting `reconciled` does not alter the record's lifecycle state and does not release
  or re-allocate quota capacity.
- Keep `fr-durable-usage-accounting` responsible for persisting records for all four lifecycle states plus the
  `reconciled` flag.
- Keep `fr-usage-summary` responsible for distinguishing quantities by lifecycle state and additionally exposing
  `reconciled` counts, but require that `reconciled` counts not be summed with the four lifecycle-state quantities
  as a fifth disjoint bucket.
- Extend `fr-shared-recovery-state-machine` to route both the four-state lifecycle transitions and the post-terminal
  setting of `reconciled` through the same canonical state machine.

### Consequences

- Good, because the reservation lifecycle remains a small, closed four-state machine with well-defined terminal
  states already reasoned about elsewhere in the PRD.
- Good, because reconciliation is decoupled from admission and settlement correctness: a worker setting `reconciled`
  cannot by accident release capacity or change a record's admission-relevant state.
- Good, because aggregation math in usage summaries is safe: the four lifecycle-state buckets remain disjoint and
  `reconciled` is an orthogonal overlay.
- Good, because the change is minimally invasive to existing requirements and does not widen the state-machine API
  surface.
- Bad, because consumers who previously read the five-element enumerations in `fr-durable-usage-accounting` and
  `fr-usage-summary` as a flat closed set must now understand the lifecycle-plus-flag split. This is mitigated by the
  explicit wording added to the affected requirements.
- Bad, because tooling that modelled the five enumerations as a flat enum will need to change to a (state, flag)
  representation.

### Confirmation

- Code review: verify the reservation record schema models a four-value state enum and a separate `reconciled`
  boolean flag, and that no transition function accepts `reconciled` as a state.
- Contract tests: verify `fr-usage-summary` responses surface both four-state quantities and `reconciled` counts,
  and that test fixtures do not sum them as five disjoint buckets.
- Integration tests: verify reconciliation worker flows set `reconciled = true` without altering lifecycle state and
  without mutating quota capacity.
- Static checks: verify no requirement or contract enumerates `reconciled` alongside the four lifecycle states as if
  it were a state.

## Pros and Cons of the Options

### Option A — `reconciled` as a post-terminal audit flag

- Good, because the lifecycle stays compact and easy to reason about under retries and partial failures.
- Good, because the division of responsibility between the consumer-facing reservation API and the reconciliation
  worker is preserved: the worker touches only the audit flag, never the lifecycle state.
- Good, because aggregate math in `fr-usage-summary` is disjoint by construction across the four lifecycle buckets.
- Bad, because every requirement that previously listed `reconciled` in the lifecycle enumeration must be reworded.

### Option B — `reconciled` as a fifth terminal lifecycle state

- Good, because consumers of the state enum see exactly one field and do not need a separate flag.
- Bad, because `committed` can no longer be uniformly treated as terminal: a `committed` record may or may not have
  already transitioned to `reconciled`, which complicates "reject settlement against a terminal reservation"
  semantics.
- Bad, because reconciliation acquires the authority to change lifecycle state, which couples audit work to
  admission-relevant state in a way that makes recovery reasoning harder.
- Bad, because an absorbing `reconciled` state invites implementations to collapse `released` and `expired` records
  into `reconciled`, losing the original terminal cause.

### Option C — Remove `reconciled` from the specification

- Good, because the PRD is simpler and every requirement mentions only four lifecycle states.
- Bad, because downstream operator tooling that relies on per-record reconciliation visibility loses a durable
  signal; reconciliation progress becomes an external-process concern only.
- Bad, because it removes the explicit link between the reconciliation worker's completion of a record and the
  durable usage record it reconciled, weakening auditability.

## Review Cadence

Revisit this decision when any of the following becomes true:

- A new reconciliation mode requires distinguishing multiple categories of reconciled records (for example,
  "reconciled against legacy authority" vs "reconciled against native authority") that cannot be represented by a
  single boolean flag.
- Operator tooling demonstrates a need to branch lifecycle handling based on `reconciled` status, which would
  indicate that `reconciled` is carrying state-machine-relevant meaning and should be elevated.
- Aggregation or billing requirements change in a way that makes a five-bucket disjoint representation more natural
  than the current four-state plus flag model.

## Traceability

- **PRD**: [../PRD.md](../PRD.md) — §5.7 Reservation Lifecycle Guarantees; §5.8 Durable Usage Accounting; §5.8 Usage
  Summary and Remaining Quota Views; §5.9 Shared Recovery State Machine
- **DESIGN**: [../DESIGN.md](../DESIGN.md) — §3.6.1 Reservation Lifecycle State Machine; §3.6.2 Shared Recovery
  State Machine
- **Related ADRs**:
  - [0001-cpt-cf-licensing-service-adr-federated-licensing-authority.md](./0001-cpt-cf-licensing-service-adr-federated-licensing-authority.md)
  - [0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md](./0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md)

This decision directly addresses the following requirements:

- `cpt-cf-licensing-service-fr-reservation-lifecycle`
- `cpt-cf-licensing-service-fr-durable-usage-accounting`
- `cpt-cf-licensing-service-fr-usage-summary`
- `cpt-cf-licensing-service-fr-shared-recovery-state-machine`
- `cpt-cf-licensing-service-fr-reconciliation`
- `cpt-cf-licensing-service-fr-release-outcome`
