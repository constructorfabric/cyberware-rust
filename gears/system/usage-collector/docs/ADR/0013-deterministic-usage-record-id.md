---
status: accepted
date: 2026-07-07
---

Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-20 by Virtuozzo International GmbH

# Deterministic gateway-derived usage-record id (UUIDv5 of the dedup key)

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [(b) Deterministic UUIDv5 from the dedup key — chosen](#b-deterministic-uuidv5-from-the-dedup-key--chosen)
  - [(a) Server-random identity + drop from canonical equality](#a-server-random-identity--drop-from-canonical-equality)
  - [(c) Keep client-supplied identity](#c-keep-client-supplied-identity)
- [More Information](#more-information)
- [Traceability](#traceability)
- [Amendment 2026-07-17 — `created_at` folded into the derivation (see ADR-0014)](#amendment-2026-07-17--created_at-folded-into-the-derivation-see-adr-0014)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-deterministic-usage-record-id`

## Context and Problem Statement

The usage-record identity was client-supplied end-to-end while the dedup key is
`(tenant_id, gts_id, idempotency_key)`, which does not include it. Nothing
enforced identity uniqueness, so distinct keys could carry the same value —
making `get`/`deactivate` operate on a non-unique key — and an exact
retry that regenerated the value was a false `IdempotencyConflict` because the
plugin's canonical-equality comparison included it. Both problems share
one root cause: identity was an independent client field rather than a function
of the dedup key.

## Decision Drivers

- `cpt-cf-usage-collector-fr-idempotency` — identity must be stable across
  at-least-once retries without making that stability a client obligation.
- `cpt-cf-usage-collector-principle-fail-closed` — a client-sent identity must
  not be silently trusted.
- The by-id `get` / `deactivate` surfaces must operate on a unique identity,
  and an exact same-key retry must not surface a false conflict.

## Considered Options

- **(a)** Server-generate a random identity (Postgres `DEFAULT gen_random_uuid()`
  or core UUIDv7) and drop it from canonical equality.
- **(b)** Derive `id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_id ⟨0x1F⟩ idempotency_key)`
  on the gateway.
- **(c)** Keep the client-supplied identity (status quo).

## Decision Outcome

Chosen: **(b)**. Identity is a deterministic projection of the dedup key,
computed for every caller — REST and in-process SDK alike — at a single
domain-service choke point. A fixed namespace constant
(`56313026-863b-4de8-b32b-1f96b67306ed`) and the derivation live in
`usage-collector-sdk` (`derive_usage_record_id`) so clients can reproduce the
value locally. The identity field is **removed from the create surface
entirely**, not merely ignored: the SDK create methods take an identity-free
`CreateUsageRecord` (mirroring `UsageRecord` minus the server-owned `id` and
`status`), and the wire request omits it likewise (`deny_unknown_fields`
rejects a stray value with `400`). `CreateUsageRecord::into_usage_record` is the
sole point that stamps the derived `id` and the initial `status = active`. On
the persisted `UsageRecord` the identity field is renamed `uuid → id`.

### Consequences

- One key ⇒ one `id` (deterministic). UUIDv5 is a truncated SHA-1 digest, so it
  is collision-resistant but not injective: distinct keys are overwhelmingly
  likely to yield distinct `id`s, but a collision is cryptographically
  negligible (~2⁻¹²²) rather than impossible by construction. The by-id `get` /
  `deactivate` surfaces therefore operate on an effectively-unique identity; the
  residual collision probability is out of scope (no runtime collision-handling
  path).
- A same-key retry yields the same `id`, so canonical equality no longer
  produces a false conflict; identity stability is no longer a client
  obligation.
- Breaking wire change: callers must stop sending the identity. Acceptable —
  the gear is in development and unreleased.
- A client MAY pre-compute a target's `id` to set `corrects_id` without a
  round-trip.

### Confirmation

Unit tests pin the namespace and golden vectors and assert determinism,
distinctness across the pinned key vectors, v5, and separator-safety; SDK tests assert
`CreateUsageRecord::into_usage_record` stamps the derived `id` and the initial
`Active` status (the create surface being identity-free by type, a caller cannot
supply an `id` in the first place); gateway tests assert the derived id is
returned, that a stray `id` is rejected `400`, and that a same-key retry yields
the same `id`.

## Pros and Cons of the Options

### (b) Deterministic UUIDv5 from the dedup key — chosen

- Good: closes both problems at the source; client-reproducible; no schema
  default needed; no plugin logic change.
- Bad: breaking wire change; namespace is forever-fixed.

### (a) Server-random identity + drop from canonical equality

- Good: also closes both problems.
- Bad: not client-reproducible (no offline `corrects_id`); needs a canonical-
  equality change in the plugin.

### (c) Keep client-supplied identity

- Bad: leaves both problems open.

## More Information

Related:
[`./0004-mandatory-idempotency.md`](./0004-mandatory-idempotency.md)
(`cpt-cf-usage-collector-adr-mandatory-idempotency`).

## Traceability

- Amends: `plugin-spi.md`, `sdk-trait.md`, `domain-model.md`,
  `features/usage-emission.md`, `features/event-deactivation.md`,
  `DECOMPOSITION.md`, PRD, DESIGN, `usage-collector-v1.yaml`.

## Amendment 2026-07-17 — `created_at` folded into the derivation (see ADR-0014)

[`ADR-0014`](./0014-created-at-in-dedup-identity.md) folds `created_at` into the
derivation:
`id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_id ⟨0x1F⟩ created_at_micros ⟨0x1F⟩ idempotency_key)`,
making `id` a projection of the 4-tuple dedup key
`(tenant_id, gts_id, idempotency_key, created_at)`. The namespace is unchanged.
The "one key ⇒ one `id`" and "same-key retry ⇒ same `id`" consequences above now
read "one 4-tuple ⇒ one `id`" and "same-4-tuple retry ⇒ same `id`"; a same-key
submission with a different `created_at` is a distinct record, not a
false-conflict source. Offline `corrects_id` pre-computation now also needs the
target's exact `created_at` (µs). Rationale and full consequences live in
ADR-0014; this Decision body is otherwise unchanged.
