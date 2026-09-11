---
status: accepted
date: 2026-07-08
---

Created:  2026-07-07 by Virtuozzo International GmbH
Updated:  2026-07-07 by Virtuozzo International GmbH

# ADR-0003: Value-Fingerprint Fence for the Metadata/Value Dual Write

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
  - [Amendment (2026-07-24): mandatory write preconditions do not obsolete the fence](#amendment-2026-07-24-mandatory-write-preconditions-do-not-obsolete-the-fence)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Serialize writers with a distributed lock](#serialize-writers-with-a-distributed-lock)
  - [`SERIALIZABLE` isolation on the metadata write](#serializable-isolation-on-the-metadata-write)
  - [Stamp the version into the backend value / key](#stamp-the-version-into-the-backend-value--key)
  - [Value fingerprint in the gear row (CHOSEN)](#value-fingerprint-in-the-gear-row-chosen)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-value-fingerprint-fence`

**Amended by [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)**: the fence is kept as an integrity check against out-of-band modification of a backend entry; its saga role — detecting a torn write between the row and the backend — no longer exists, because a row points only at bytes that were fully written before the pointer moved. The healing `If-Match: *` re-put is withdrawn, and so is the seeding mode that let a row with a value carry `value_fp IS NULL`, served on trust; a fingerprint now exists for every value and for nothing else — a value-less (`declared`) row still carries none, tied to `value_id`'s own nullability by `ck_credstore_fp_with_value`.

## Context and Problem Statement

With a stateful gear ([ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md)) a write spans two stores with no shared transaction: the value goes to the external value-store backend (`plugin.put`), the metadata (`sharing`, `version`, `expires_at`) to `credstore_secrets` (`touch`). On the last-writer-wins path (no `If-Match`) both a backend write and a metadata write happen, so two concurrent PUTs to one reference can interleave crosswise:

```
Alice: PUT value=A, sharing=shared     Bob: PUT value=B, sharing=tenant
  plugin.put(A)                           plugin.put(B)   → backend = B
  touch(sharing=shared)                   touch(sharing=tenant)
  # commit order: put(A), put(B), touch(Bob=tenant), touch(Alice=shared)
  # end state:   backend = B             row.sharing = shared
```

A descendant tenant then resolves Bob's value under Alice's `shared` label — a durable cross-tenant disclosure of a secret its writer marked tenant-only (review finding #2). Independently, the `ETag` was the bare `version` counter, which restarts at `1` for every recreated row, so a stale client's `If-Match: "1"` matched a *different generation* and overwrote it — the ABA lost update the strong validator exists to prevent (finding #1).

Optimistic concurrency (the `If-Match`/version precondition) does not by itself close the crosswise disclosure. It is opt-in — the last-writer-wins path above carries no `If-Match` at all — and even when a client supplies one it guards only the *metadata row's* version, within a single store. It cannot bind the backend value to the metadata across two transaction-less stores, so it never detects the failure here: the surviving row's `sharing` label was set by a different writer than the one whose value survived in the backend. Serializing the two writes would need a fence the platform's primitives do not provide:

The platform's coordination primitives are explicitly not built to fence this: the cluster distributed lock has **no fencing tokens** and forbids remote I/O inside a lock's critical section (cluster ADR-002), and the `coord` DB-lease documents the same non-guarantee for external side effects. How do we make the metadata and the value provably consistent on read without holding a lock across the backend write, and without a bare-version validator?

## Decision Drivers

* No value under a sharing label a different writer set — fail closed, never disclose
* Correct across replicas (the gear is multi-instance) without new infrastructure
* Do not hold any lock across the external `plugin.put` (cluster ADR-002)
* Nothing derived from a secret value may leave the gear (no fingerprint on the wire, in headers, or in logs)
* The backend stays an opaque byte store — the same mechanism works for the in-memory static plugin and any future vault-backed plugin, with no plugin-contract change
* Close the ABA lost update in the same change
* Fail-closed and self-healing under every partial-failure and key-loss mode

## Considered Options

* Serialize writers with a distributed lock (cluster / `coord`)
* `SERIALIZABLE` isolation on the metadata write
* Stamp the version into the backend value (inline framing) or backend key
* Value fingerprint (`HMAC-SHA256(fence_key, value)`) in the gear row, verified on read

## Decision Outcome

Chosen option: **"Value fingerprint in the gear row"**. Each row stores `value_fp = HMAC-SHA256(fence_key, value)`, stamped in the same atomic `touch`/insert as `sharing`; reads recompute it from the value the backend returned and serve only on a match, else fail closed as an anti-enumeration
404. The atomic co-write of `value_fp` and `sharing` is what proves a matched
value and its metadata came from one writer. The fence key is auto-generated and stored in the backend under a reserved, API-unreachable entry (split knowledge: fingerprints in the DB, key with the values). The `ETag` becomes the generation-bound pair `"<row-id>.<version>"`, closing the ABA. This is precisely the application-level fencing cluster ADR-002 prescribes for an "external storage layer with fencing support" — here the gear supplies that support on the backend's behalf. See §4.10 of DESIGN.

### Consequences

* A crosswise LWW interleave, a half-completed overwrite, or a lost/replaced fence key can only ever produce a fingerprint mismatch → 404, healed by any subsequent successful PUT. No state serves a value under mismatched metadata.
* LWW is no longer strict last-writer-wins under a same-value/different-sharing race: the loser's intent can be lost. This is not a disclosure — a served value's sharing label was set by the writer of *that* value (the atomic co-write), so no value is ever exposed under a sharing nobody assigned it.
* Backend value bytes and the plugin contract are unchanged; a future vault-backed plugin needs no fencing support of its own.
* A value-derived artifact (the keyed HMAC) now lives at rest in the gear DB. It never leaves the gear, and the HMAC key (not in the DB) blocks offline dictionary attack on a DB-only compromise.
* A new dependency surface: the fence key must be reachable at write/read time. Its outage coincides with a backend outage (same store), which already fails the operation; a lost/rotated key fails closed (loud `fence_verify{outcome="mismatch"}`) and re-PUT heals.
* Out-of-band DB seeding must set `value_fp = NULL` (and reset it on re-seed); such rows are served on trust and backfilled on first read / reaper sweep.
* Key rotation is out of scope; the `fp_key_id` column and reserved-reference naming are the groundwork (a keyring verified by id, lazy re-stamp) for a later change.

### Confirmation

* Unit test: the poisoned crosswise end-state (backend value ≠ row fingerprint) reads as a 404, and a subsequent PUT heals it.
* Unit + e2e: a recreated secret rejects the previous generation's `"<id>.<version>"` validator (409) even though the version counters coincide.
* Unit: an out-of-band `value_fp = NULL` row serves on trust, backfills exactly once via a CAS that does not bump the version, then verifies `ok`.
* Unit: fence-key bootstrap persists the key under the reserved nil-tenant entry; a tenant PUT to the same reference never clobbers it and cannot resolve it.
* Unit: a stale cached key self-heals via the one-shot refresh on mismatch.
* Metrics: `fence_verify{ok|legacy|mismatch}` and `fence_backfill{...}` are emitted with the documented labels.

### Amendment (2026-07-24): mandatory write preconditions do not obsolete the fence

The write API later made the optimistic-concurrency precondition **mandatory** on update/delete (PR #4204 review): there is no bare unconditional PUT anymore — every update carries either a version validator (CAS, claimed metadata-first, so a losing writer never reaches the backend) or an explicit `If-Match: *` (last-writer-wins, the opt-in for rotation/provisioning flows that hold no version). This narrows the crosswise window but does not close the problem this ADR solves, so the fence stays, for three independent reasons:

* **`Exists` writers still race crosswise.** `If-Match: *` remains part of the contract (it is unavoidable — see the healing point below), and two `*` writers interleave exactly like the original LWW pair. The fence is what turns that from a disclosure into a fail-closed 404.
* **A precondition cannot bind two transaction-less stores.** Even on the version-gated path the CAS guards only the metadata row. A crash or plugin failure *between* the committed `touch` (new `sharing`, new fingerprint) and the backend `plugin.put` leaves the backend holding the previous writer's value under the new writer's label — the exact cross-writer mismatch, produced with no concurrency at all. Only the read-side recompute-and-compare detects it. The same applies to operational drift the API never sees: a backend restored from backup, out-of-band seeding, a replaced fence key.
* **The fence is the healing contract's other half.** A poisoned reference reads as an anti-enumeration 404, so no validator can be obtained for it; the recovery write is precisely the `If-Match: *` PUT. Dropping `Exists` in favour of version-only preconditions would make poisoned references unrecoverable through the API; dropping the fence in favour of preconditions would silently serve mismatched state. The two mechanisms are complements, not alternatives.

Consequence for the original text: the "LWW path" described in the context above is now spelled `If-Match: *` and is a deliberate caller choice instead of the default; the create saga is unchanged (creation remains the only preconditionless write).

## Pros and Cons of the Options

### Serialize writers with a distributed lock

Hold a cluster / `coord` lock per `(tenant, ref)` across `plugin.put` + `touch`.

* Bad, because both primitives forbid exactly this (no remote I/O in a lock critical section; no fencing tokens — cluster ADR-002), so a TTL-preempted holder still races.
* Bad, because it needs shared infrastructure (a linearizable cache / DB-lease table) and a per-write acquire on the hot path.
* Bad, because even a perfect lock cannot make a two-store write atomic; a pause between the two writes still desyncs.

### `SERIALIZABLE` isolation on the metadata write

* Bad, because isolation only serializes operations *within* the DB; the external `plugin.put` is outside any transaction, so the value/metadata desync is untouched.
* Bad, because holding a transaction open across the backend network call is a long-transaction anti-pattern and still does not order the external write.

### Stamp the version into the backend value / key

Inline-frame `magic‖version‖value`, or route each version to a distinct backend key.

* Good, because it is airtight and keeps no value-derived data in the DB.
* Bad, because it mutates the stored secret (a direct backend reader sees framed bytes) or changes the key scheme and accumulates old-version values needing GC.
* Bad, because "is there a stamp?" becomes byte-sniffing a magic prefix (collision-prone) instead of a clean typed check; and for a structured backend (e.g. OpenBao KV) it either mangles the value field or needs a plugin-contract change.

### Value fingerprint in the gear row (CHOSEN)

* Good, because the backend and the plugin contract are untouched — one mechanism for every backend.
* Good, because verification is a clean recompute-and-compare, and "unstamped" is a NULL column, not a guessed prefix.
* Good, because it is the exact pattern cluster ADR-002 points external-resource fencing to (application-level), and it closes the ABA in the same `ETag` change.
* Neutral, because it stores a keyed HMAC of the value at rest — mitigated by keying and by never exposing it.
* Bad (accepted), because it does not preserve strict LWW under a same-value/different-sharing race (a lost update, never a disclosure).

## Traceability

- Requirements: `cpt-cf-credstore-fr-put-secret`, `cpt-cf-credstore-fr-get-secret`, `cpt-cf-credstore-fr-sharing-modes`, `cpt-cf-credstore-fr-optimistic-concurrency`, `cpt-cf-credstore-nfr-tenant-isolation`, `cpt-cf-credstore-nfr-confidentiality`
- Supersedes the bare-version `ETag` described in earlier revisions of DESIGN §4.3; builds on the dual-write model of [ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md) / [ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md).
- Related: cluster ADR-002 (`gears/system/cluster/docs/ADR/002-async-boundary-no-remote-in-critical-section.md`).
