---
status: proposed
date: 2026-04-20
decision_owner: Platform Engineering
approver: Architecture Review Board
scope: licensing-service; all ModKit modules integrating quota reservation and commit/release settlement
---

# ADR-0004: Online Idempotency TTL — Module-Wide vs Per-Policy Scope

**ID**: `cpt-cf-licensing-service-adr-online-idempotency-ttl-scope`

- **Priority applicability**: p2

## Context and Problem Statement

The Licensing Service quota layer relies on online idempotency records to guarantee replay-safe settlement after
network retries, consumer restarts, and late `Commit` / `Release` arrivals. `cpt-cf-licensing-service-fr-idempotent-lifecycle`
requires that the previously accepted result be returned on replay, and `cpt-cf-licensing-service-fr-idempotency-vs-archive`
requires that the online idempotency lookup be kept separate from archive retention and be retained at least as
long as replay-safe settlement can be expected.

The document specifies that a late `Commit` after reservation expiry must match the original idempotency record
and return `error_code = reservation_expired` rather than creating a duplicate reservation. This establishes a
lower bound on idempotency TTL: `reservation_ttl + settlement_grace_period`.

The remaining question is whether `settlement_grace_period` (and therefore the total idempotency TTL) should be a
single module-wide value or a per-policy override. The two options have different operational, storage, and
configuration consequences.

## Decision Drivers

- Minimize p1 operational surface area: one TTL value is simpler to reason about, monitor, and migrate than many.
- Minimize fast-path storage complexity: Redis-class stores apply a single TTL per key; a per-policy TTL requires
  the fast-path store to look up the applicable policy on each idempotency write or to carry the TTL in the key
  shape.
- Preserve the ability to differentiate grace periods per policy later if one policy family exhibits materially
  different settlement timing.
- Do not couple idempotency TTL semantics to policy lifecycle events (policy rename, retire, merge) in p1.
- Preserve correctness of the lower bound `reservation_ttl + settlement_grace_period` under every configuration.

## Considered Options

- **Option A — Module-wide constant.** A single `idempotency_ttl_seconds` applies to every quota policy.
  `settlement_grace_period_seconds` is a single module-wide constant (default `300`, range `[0, 3600]`).
  `idempotency_ttl_seconds` is computed as `max_reservation_ttl_seconds + settlement_grace_period_seconds` and is
  evaluated once at module startup.
- **Option B — Per-policy override.** Each quota policy MAY declare its own
  `settlement_grace_period_seconds`. Effective idempotency TTL per record is
  `policy.default_reservation_ttl_seconds + policy.settlement_grace_period_seconds`. The fast-path store writes
  each idempotency record with the policy-specific TTL.
- **Option C — Fixed 24 hours without formula.** Set `idempotency_ttl_seconds = 86400` unconditionally, without a
  formula linking it to reservation TTL or grace period. Callers relying on late settlement after `max` reservation
  TTL see `idempotency_record_expired` before `reservation_expired`, which is incorrect for the replay-safety
  requirement.

## Decision Outcome

Chosen option: **Option A — Module-wide constant.**

This ADR is not a p1 exit criterion for minimal enforcement; it becomes normative when the module adopts the
stricter p2 replay-retention model.

The Licensing Service p1 uses a single module-wide `idempotency_ttl_seconds` computed from
`max_reservation_ttl_seconds + settlement_grace_period_seconds`. Terminology: `max_reservation_ttl_seconds` is a
**module-wide hard cap** (per DESIGN.md §3.6.1), not an overridable default; it cannot be raised by operator
configuration in p1. `settlement_grace_period_seconds` is a **module-wide constant with an operator-overridable
default** of `300` seconds, valid range `[0, 3600]`. With the p1 hard cap (`86400`) and the grace-period default
(`300`) the total is `86400 + 300 = 86700` seconds (≈24 hours 5 minutes). Because `idempotency_ttl_seconds` is
evaluated once at module startup (see Decision Drivers above), any future change to either
`max_reservation_ttl_seconds` (requires a major version bump) or `settlement_grace_period_seconds` (operator
override) takes effect only at the next module startup; the fast-path idempotency writer must not cache a stale
value across a configuration change.

Policy configuration does not override grace period in p1. Per-policy grace (Option B) is a non-breaking future
extension: the storage key shape already embeds the idempotency key, and switching from a module-wide TTL to a
per-record TTL is a storage-layer change that does not require changes to the public API, the reservation record
schema, or the PRD invariants. The migration path is recorded in the Review Cadence section below so it can be
revisited if operational evidence requires it.

### Consequences

- Good, because p1 operates with one TTL value that is easy to monitor, alert on, and explain.
- Good, because fast-path storage uses a single TTL per key shape, avoiding the need to read policy configuration
  on every idempotency write.
- Good, because the lower bound `reservation_ttl + settlement_grace_period` is satisfied uniformly for every
  policy.
- Good, because moving to Option B later is non-breaking for storage and public contracts; only the policy config
  surface and the TTL evaluation code change.
- Bad, because policies whose real settlement latency is materially longer or shorter than the module-wide grace
  period cannot tune it in p1. Operators can still tune `settlement_grace_period_seconds` globally.
- Bad, because a single module-wide grace period forces the module to pick a value that protects the slowest
  policy, which may slightly over-retain idempotency records for fast policies.

### Confirmation

- Code review: verify the fast-path idempotency writer uses a single TTL constant derived from the module-wide
  configuration and does not read per-policy grace period.
- Contract test: a `Commit` arriving after reservation expiry but within `settlement_grace_period_seconds` MUST
  return `reservation_expired` (matched to its idempotency record), not a new reservation.
- Contract test: a `Commit` arriving after `idempotency_ttl_seconds` MUST return `idempotency_record_expired`.
- Operational metric: alert on idempotency-store retention drift that would drop records before
  `max_reservation_ttl_seconds + settlement_grace_period_seconds`.

## Pros and Cons of the Options

### Option A — Module-wide constant

- Good, because the configuration surface is minimal and the fast-path TTL computation is trivial.
- Good, because the TTL lower bound is satisfied for every policy by construction.
- Good, because a single metric suffices to observe idempotency record lifetime in production.
- Bad, because policies with unusually fast or slow real-world settlement cannot tune grace independently.

### Option B — Per-policy override

- Good, because grace can be tuned precisely per policy family.
- Bad, because each idempotency write must resolve policy configuration (either the write site reads policy, or
  the key shape embeds policy identity; both add complexity).
- Bad, because policy lifecycle operations (rename, retire, migrate) now affect the interpretation of live
  idempotency records, which creates subtle correctness risks during policy reconfiguration.
- Bad, because operator dashboards and alerts must account for many TTLs rather than one.

### Option C — Fixed 24 hours without formula

- Good, because the value is trivial to remember.
- Bad, because it does not guarantee the lower bound `reservation_ttl + settlement_grace_period` when reservation
  TTL itself is at its maximum. Late `Commit` at the boundary of `expires_at` could fall outside the idempotency
  window, which directly violates `cpt-cf-licensing-service-fr-idempotency-vs-archive`.
- Bad, because the semantics are accidental rather than derived from the replay-safety requirement.

## Review Cadence

Revisit this decision when any of the following becomes true:

- Production telemetry shows that a meaningful fraction of legitimate late settlements arrive after the
  module-wide grace period and are rejected as `idempotency_record_expired`.
- A policy family emerges whose real-world settlement latency is consistently outside the module-wide grace
  window (for example, a batch settlement pipeline that submits `Commit` hours after the original reservation).
- Operator experience indicates that per-policy tuning of grace would materially reduce false-positive alerts or
  storage retention costs.

When any of the above trigger a revisit, the migration path is: (1) move `settlement_grace_period_seconds` from a
module-wide constant to a quota-policy field (with the current module-wide value as the fallback for unset
policies); (2) change the fast-path idempotency writer to derive TTL per write from the applicable policy; (3)
update operator dashboards to report the TTL distribution. The PRD invariant in
`cpt-cf-licensing-service-fr-idempotency-vs-archive` remains unchanged because it is expressed in terms of the
lower bound, not a specific scope.

## Traceability

- **PRD**: [../PRD.md](../PRD.md) — §5.8 `cpt-cf-licensing-service-fr-idempotency-vs-archive`
- **DESIGN**: [../DESIGN.md](../DESIGN.md) — §3.4 Online Idempotency TTL; §3.6.1 Reservation TTL Mechanics
- **Related ADRs**:
  - [0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md](./0002-cpt-cf-licensing-service-adr-unified-licensing-quota-module.md)
  - [0003-cpt-cf-licensing-service-adr-reservation-lifecycle-and-reconciled-semantics.md](./0003-cpt-cf-licensing-service-adr-reservation-lifecycle-and-reconciled-semantics.md)

This decision directly addresses the following requirements:

- `cpt-cf-licensing-service-fr-idempotency-vs-archive`
- `cpt-cf-licensing-service-fr-idempotent-lifecycle`
- `cpt-cf-licensing-service-fr-check-and-reserve`
- `cpt-cf-licensing-service-fr-reservation-lifecycle`
- `cpt-cf-licensing-service-fr-persisted-grant-update`
