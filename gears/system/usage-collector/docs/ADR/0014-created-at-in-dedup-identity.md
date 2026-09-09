---
status: accepted
date: 2026-07-17
---

Created:  2026-07-20 by Virtuozzo International GmbH
Updated:  2026-07-20 by Virtuozzo International GmbH

# `created_at` as part of the usage-record dedup identity (4-tuple)


<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [(A) 4-tuple canonical identity — chosen](#a-4-tuple-canonical-identity--chosen)
  - [(B) Plugin 3-tuple side table](#b-plugin-3-tuple-side-table)
  - [(C) Keep the divergence](#c-keep-the-divergence)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-usage-collector-adr-created-at-in-dedup-identity`

## Context and Problem Statement

ADR-0013 derives the record identity as
`id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_id ⟨0x1F⟩ idempotency_key)` — a projection
of the **3-tuple** dedup key — on the explicit assumption that the dedup key is
that 3-tuple (ADR-0004). A TimescaleDB hypertable `UNIQUE` must include the
partition column (`created_at`), so the TimescaleDB storage plugin dedups on the
**4-tuple** `(tenant_id, gts_id, idempotency_key, created_at)`. The two
disagree: the same 3-tuple submitted with two different `created_at` values
persists as two live rows that share one derived `id`, so `get_usage_record`
returns an arbitrary one and `deactivate_usage_record` (`WHERE id = $1`) flips
all of them. ADR-0013 explicitly declined a runtime id-collision path on the
3-tuple assumption the 4-tuple dedup does not satisfy.

## Decision Drivers

- The by-`id` `get` / `deactivate` surfaces MUST address exactly one stored row.
- A hypertable-based plugin cannot express a time-independent 3-tuple `UNIQUE`;
  requiring one forces a ~1:1 side table (rejected for v1; TimescaleDB plugin
  `DESIGN.md` §2.2).
- The identity must stay a deterministic, client-reproducible projection of the
  dedup key (ADR-0013's offline `corrects_id` property).

## Considered Options

- **(A)** Adopt the 4-tuple as the canonical dedup identity: fold `created_at`
  into the derivation (`id = UUIDv5(4-tuple)`) and make `created_at` part of the
  dedup key rather than a canonical field.
- **(B)** Keep the 3-tuple identity and give the plugin a normal side table with
  `UNIQUE (tenant_id, gts_id, idempotency_key)`.
- **(C)** Keep the documented divergence (status quo): accept the id collision.

## Decision Outcome

Chosen: **(A)**. `created_at` becomes part of the record identity. The
derivation is
`id = UUIDv5(NS, tenant_id ⟨0x1F⟩ gts_id ⟨0x1F⟩ created_at_micros ⟨0x1F⟩ idempotency_key)`,
where `created_at_micros` is the event timestamp as integer
microseconds-since-epoch (the precision Postgres `timestamptz` stores and the
plugin dedups on) and `idempotency_key` remains the final field so the encoding
stays injective. The fixed namespace (`56313026-863b-4de8-b32b-1f96b67306ed`)
is unchanged. `created_at` is caller-supplied, so a client still reproduces the
id offline. The dedup key is now the 4-tuple
`(tenant_id, gts_id, idempotency_key, created_at)`; `created_at` is removed from
the canonical-field comparison set (`value`, `resource_ref`, `subject_ref`,
`corrects_id`, `metadata`).

### Consequences

- One 4-tuple ⇒ one `id`; `get`/`deactivate` address exactly one row. The
  TimescaleDB plugin's 4-tuple dedup is now the canonical behavior, not a
  divergence.
- **Same idempotency key + different `created_at` is two distinct records**
  (distinct ids), not an `IdempotencyConflict`. The "loud conflict on a
  different event timestamp" billing-protection signal ADR-0004 specified is
  intentionally dropped. Same key + same `created_at` + any *other* canonical
  field differing is still an `IdempotencyConflict`; key-reuse-with-different-
  data is still caught.
- Offline `corrects_id` pre-computation now requires the target's exact
  `created_at` (at µs precision) in addition to the 3-tuple.
- The gear is unreleased/greenfield; the formula change re-mapping every id
  breaks no stored data (no backfill).
- The plugin's separate retention-bounded key-preservation divergence from
  ADR-0004 (chunk lifecycle vs. permanent) is unaffected and remains a
  documented divergence.

### Confirmation

SDK unit tests pin the new golden vectors and assert determinism, distinctness
by `created_at`, µs-truncation, v5, and separator-safety with the new field
order; SDK tests assert `into_usage_record` stamps the 4-tuple id and normalizes
`created_at` to µs; gateway tests assert same-key/different-`created_at` yields
distinct ids; a plugin integration test asserts two such rows persist with
distinct ids and that `get`/`deactivate` each affect exactly one.

## Pros and Cons of the Options

### (A) 4-tuple canonical identity — chosen

- Good: closes the collision at the source; no plugin logic change; identity
  stays deterministic and client-reproducible; TimescaleDB plugin becomes
  conformant.
- Bad: drops the same-key/different-timestamp conflict signal everywhere;
  offline `corrects_id` now needs the exact `created_at`.

### (B) Plugin 3-tuple side table

- Good: preserves the loud conflict and a 3-tuple identity.
- Bad: ~1:1 storage with `usage_records` (hundreds of billions of rows at the
  NFR envelope), prunable only by a daily anti-join. Rejected for v1.

### (C) Keep the divergence

- Bad: leaves `get`/`deactivate` operating on a non-unique id.

## More Information

Amends [`./0004-mandatory-idempotency.md`](./0004-mandatory-idempotency.md)
(`cpt-cf-usage-collector-adr-mandatory-idempotency`) and
[`./0013-deterministic-usage-record-id.md`](./0013-deterministic-usage-record-id.md)
(`cpt-cf-usage-collector-adr-deterministic-usage-record-id`).

## Traceability

- Amends: `ADR-0004`, `ADR-0013`, `plugin-spi.md`, `domain-model.md`,
  `DESIGN.md`, `features/usage-emission.md`, `usage-collector-v1.yaml`, and the
  TimescaleDB plugin `DESIGN.md` §2.2. `ADR-0011` reviewed; dedup-tuple
  references updated to the 4-tuple where they name the key.
