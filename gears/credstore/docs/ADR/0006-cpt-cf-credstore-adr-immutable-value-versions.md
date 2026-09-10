---
status: proposed
date: 2026-09-10
---

Created:  2026-09-10 by Constructor Tech
Updated:  2026-09-10 by Constructor Tech

# ADR-0006: Immutable Value Versions with a Pointer in the Row

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [The protocol](#the-protocol)
  - [The gc table is the intent log](#the-gc-table-is-the-intent-log)
  - [Statuses: two, not four](#statuses-two-not-four)
  - [What the fence still does](#what-the-fence-still-does)
  - [Preconditions: one ordering](#preconditions-one-ordering)
  - [Name retention is no longer needed](#name-retention-is-no-longer-needed)
  - [Backend key shape](#backend-key-shape)
  - [The pattern, and why the reaper goes with the saga](#the-pattern-and-why-the-reaper-goes-with-the-saga)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Backward Compatibility by Mode](#backward-compatibility-by-mode)
- [Impact Analysis by Domain](#impact-analysis-by-domain)
  - [Security](#security)
  - [Authentication and authorization](#authentication-and-authorization)
  - [Integration and contract](#integration-and-contract)
  - [Operations](#operations)
  - [Performance](#performance)
  - [Reliability](#reliability)
  - [Data](#data)
  - [Maintainability and evolution](#maintainability-and-evolution)
  - [Compliance](#compliance)
  - [Not applicable](#not-applicable)
- [Revisit Triggers](#revisit-triggers)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [O1: in-place overwrite, saga, fence (shipped)](#o1-in-place-overwrite-saga-fence-shipped)
  - [O2: immutable versions, pointer, gc table (CHOSEN)](#o2-immutable-versions-pointer-gc-table-chosen)
  - [O3: backend-native versioning](#o3-backend-native-versioning)
  - [O4: two-phase commit, outbox](#o4-two-phase-commit-outbox)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-immutable-value-versions`

## Context and Problem Statement

**What ships today** ([ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md), [ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md), [ADR-0003](0003-cpt-cf-credstore-adr-value-fingerprint-fence.md)). The backend key for a value is `(tenant_id, reference, key-class)`, deterministic and reused: a rotation overwrites the same key in place. A write is a saga spanning two stores with no shared transaction, tracked by a lifecycle `status` on the metadata row — `provisioning` (1) while the backend put may not yet have landed, `active` (2) once it has, `deprovisioning` (3) while a delete's backend cleanup may not yet have landed, and `declared` (4), planned by [ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md) for a row with no value. `value_fp = HMAC(fence_key, value)`, stamped on the row and recomputed on every read, detects a row/backend mismatch and fails the read closed — a canonical 404 — until a subsequent `If-Match: *` PUT re-injects the correct bytes and heals it. Two overwrite orderings coexist: a guarded write (`If-Match: "<id>.<version>"`) CASes the row first and writes the backend second; an `If-Match: *` write, having no version to gate on, writes the backend first and the row second. `deprovisioning` retains the row's unique name until the backend delete completes, because releasing the name earlier would let a successor's `PUT` write a new value under the identical backend key while the old saga's lagging delete was still in flight — erasing the successor's value. A reaper sweeps rows stuck in `provisioning` or `deprovisioning` past a timeout. Out-of-band seeding — a row inserted directly with `value_fp IS NULL` — is supported: such a row is served on trust and its fingerprint backfilled on first read.

ADR-0004 does not touch this layer; it builds a new API on top of it. But designing that surface's write semantics — atomic create, atomic suppression, crash-safe value removal — pressed on the saga underneath hard enough to expose four failure modes that were always latent in the shipped design and are now directly reachable through the address and precondition surface ADR-0004 defines:

1. **A torn write reads as absence until healed.** A crash or a plugin fault between the backend `put` and the row's `touch` (or the reverse, depending on ordering) leaves the fence mismatched; every read of that reference fails closed as a 404 indistinguishable from a name that was never used, until some caller happens to re-`PUT` the value.
2. **A stuck `provisioning` row wedges the name it holds.** Nothing can create under that reference again until the row either completes or the reaper's timeout expires and sweeps it — an availability cost with no owner but the clock.
3. **`deprovisioning` holds a name hostage to a race that exists only because the key scheme creates it.** The entire mechanism ADR-0002 built — a lifecycle status, a reaper duty, a documented `Bad` consequence ("a reference stays unavailable for re-creation for up to `deprovisioning_timeout_secs`") — exists to guard a hazard that is a direct consequence of one design choice: the backend key is reused across a delete and the create that follows it.
4. **Two overwrite orderings is two mental models for one operation.** A reviewer has to know which precondition a request carries before they can say which store gets written first, and the fence exists, by ADR-0003's own amendment, specifically because a precondition "cannot bind two transaction-less stores" — an admission that ordering alone was never going to be enough.

All four modes share one root cause: the row and the backend can each be observed independently, mid-write, in states that disagree about which bytes belong to a reference. Detection (the fence) and containment (the saga, the reaper, name retention) are three different mechanisms built to manage that disagreement after the fact. The question this ADR answers: **can the row/backend disagreement be made impossible rather than detected?**

## Decision Drivers

- **D1 — no state in which the row points at bytes that are not the bytes it describes.** Not "detected and failed closed" — impossible to reach.
- **D2 — a failure between the stores may cost garbage, never a wrong value, a closed read, or a wedged name.** Every one of the four failure modes above costs a reader or a writer something beyond disk space; none of that cost is acceptable going forward.
- **D3 — one write ordering, regardless of precondition.** A reviewer, and the code, should not have to branch on which precondition a request carries to know which store is written first.
- **D4 — no name retention, no lifecycle statuses beyond "has a value" and "has none".** A row's status should answer one question, not narrate an in-flight saga.
- **D5 — the plugin stays a dumb kv store: no list, no transactions.** ADR-0001's promise — "any dumb per-tenant key-value store should qualify as a backend plugin" — is not spent to buy this decision.
- **D6 — reads keep one SQL query plus one backend read.** ADR-0001's resolution-cost property is not reopened.
- **D7 — no resident background loop.** Nothing in the gear runs on a timer; hygiene — garbage and expired rows — is a periodic job whose cadence an operator picks, and no correctness property depends on when it runs.

## Considered Options

- **O1** — in-place overwrite, saga, and fence (the shipped design; kept as the baseline for comparison).
- **O2** — immutable value versions: every write lands under a fresh, backend-unique key; the row holds a pointer; a garbage-collection table tracks versions no row references any more.
- **O3** — backend-native versioning: lean on a backend's own version history (Vault KV v2, GCP Secret Manager `SecretVersion`) instead of building versioning into the credstore/plugin contract.
- **O4** — two-phase commit or an outbox pattern coordinating the row and the backend as a single distributed transaction.

## Decision Outcome

**Chosen: O2 — immutable value versions with a pointer in the row.** Every value write mints a fresh `value_id` (a UUID v4) and writes the bytes to the backend under `(tenant_id, value_id)` — a plugin composes the path `tenant_id/value_id`. No `reference`, no key-class: `value_id` is unique across the whole store, so the row alone knows which value belongs to which reference and which sharing class, and the backend needs neither (see [Backend key shape](#backend-key-shape)). A backend entry, once written, is never overwritten: a version is immutable. `credstore_secrets` gains `value_id UUID NULL`, the pointer to the row's current version, with a unique partial index `uq_credstore_value_id ON credstore_secrets (value_id) WHERE value_id IS NOT NULL`; `value_id IS NULL` is `declared`, unchanged in meaning from ADR-0004. A new table, `credstore_value_gc`, records every version a write ever creates or retires, and is what lets a periodic maintenance job reconcile a plugin that cannot list:

```sql
CREATE TABLE credstore_value_gc (
    value_id     UUID PRIMARY KEY,
    tenant_id    UUID NOT NULL,
    reason       SMALLINT NOT NULL CHECK (reason IN (1, 2, 3, 4)),
    enqueued_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_credstore_value_gc_enqueued ON credstore_value_gc (enqueued_at);
```

`reason`: `1` = pending (a write's intent, in flight), `2` = superseded (replaced by a newer version), `3` = removed (the value was removed or the record deleted), `4` = aborted (a write lost its CAS) — SMALLINT codes with a `CHECK`, the convention this platform already applies to a small closed enum (`fallback`, `target_mode`; header of `gears/system/types-registry/types-registry/src/infra/storage/entity/enums.rs`).

The deciding arguments are D1 through D7 taken together, not any one of them alone: O1's fence satisfies none of them by construction — it is a detector, and every driver above asks for something a detector cannot give. O3 and O4 are rejected on grounds independent of correctness (see [Pros and Cons](#pros-and-cons-of-the-options)), which leaves O2 as the only option that clears every driver at once.

### The protocol

**Write** (`PUT` create, `PUT` replace, `PATCH` carrying `value`):

1. Validate the body, authorize (`write` and/or `write_secret`, whichever the body requires — ADR-0004), resolve the concrete type, and read the row for the precondition.
2. Insert the intent: `credstore_value_gc(new_id, tenant_id, reason = pending)`. This row is what makes a crash from this point on recoverable: without it, a bytes-only backend write with no durable trace would be garbage no periodic job could ever find, since the plugin offers no listing (D5).
3. `plugin.put(tenant_id, new_id, value)` — the bytes land under a key nothing yet points to.
4. One database transaction, the CAS: on create, `INSERT` the row `active` with `value_id = new_id` (a conflict on the reference's own create-only uniqueness — ADR-0004's `If-None-Match: *` semantics — is 409; `new_id` is fresh, so it never collides on `uq_credstore_value_id` itself); on replace, `UPDATE … SET value_id = new_id, version = version + 1, value_fp = …, fp_key_id = …, <metadata fields> WHERE id = ? AND version = ?` (unconditional on `version` for `If-Match: *`; 0 rows affected = 409). In the same transaction: `DELETE gc(new_id)` — the intent is fulfilled — and, if the row pointed at an old value, `INSERT gc(old_id, reason = superseded)`.
5. Best-effort `plugin.delete(tenant_id, old_id)` followed by best-effort `DELETE gc(old_id)`, run immediately after the commit. **This step is the cleanup path**: an ordinary, uncontested write never leaves garbage behind it, because this step removes the superseded version itself, in-line, before the request even returns. Garbage exists only when this best-effort step fails, or the write crashes before reaching it; either way the gc row for `old_id` is left `superseded`, for the periodic maintenance job's drain to find on its next run.

| Failure | State after | What readers see | Who cleans up |
|---|---|---|---|
| Steps 1–2 fail (validation, authorization, or the intent insert) | No bytes written, no row changed | Unaffected — the prior value, if any, keeps serving | Nothing to clean up |
| Step 3 fails (`plugin.put` errors or times out) | gc row for `new_id` stays `pending`; row unchanged | Unaffected — the prior value keeps serving | Best-effort `DELETE gc(new_id)`; caller gets 503 |
| Step 4 fails — DB unavailable | Backend holds `new_id`'s bytes; gc row for `new_id` stays `pending`; row unchanged | Unaffected by this write; 503 | The periodic job's pending-reclaim pass, once the entry is older than `gc.pending_max_age_secs` with no row referencing `new_id`: `plugin.delete` + `DELETE gc` |
| Step 4 fails — CAS lost | Backend holds `new_id`'s bytes, unreferenced; row already points at the winner's version | Unaffected — the winner's value serves; caller gets 409 | Best-effort `UPDATE gc(new_id) reason = aborted`, then best-effort `plugin.delete(new_id)` and `DELETE gc(new_id)`; the periodic job's gc drain as a backstop |
| Step 5 fails (best-effort cleanup of the superseded version) | Row already points at `new_id`; old version's bytes remain, gc row for `old_id` stays `superseded` | Unaffected — `new_id` serves | The periodic job's gc drain, on its next run: `plugin.delete(old_id)` + `DELETE gc(old_id)` |
| Crash at any point | At most one orphaned version and its gc row | Never a pointer to missing bytes, never a wrong value, never a wedged name | The periodic maintenance job, on its next run |

Two guarded writers racing one reference: the first CAS commits, the second's `WHERE id = ? AND version = ?` matches nothing, 409; its already-written backend bytes are marked `aborted` and deleted, both best-effort, with the periodic job's gc drain as the backstop — and if even the mark was lost, the pending-reclaim pass finds them still `pending`. Two `If-Match: *` writers racing one reference: both complete step 3 independently — two distinct `value_id`s, no collision possible — and both step-4 transactions succeed unconditionally; whichever commits last leaves its `value_id` as the pointer, and the loser's now-unreferenced version is enqueued `superseded` in the same transaction that moved the pointer past it. No clobbering, because each version lives under its own key; no fence mismatch, because the fence was stamped against the exact bytes the winning transaction wrote.

**Read**: row → `value_id` → `plugin.get(tenant_id, value_id)` → recompute and compare `value_fp` → serve. If `plugin.get` returns `NotFound` — the row was read a moment before a concurrent write switched the pointer and deleted the old version — the gear re-reads the row once and retries against its now-current `value_id`; a second miss is a real error (the version the row names is itself missing), not a race. `value_id IS NULL` (`declared`) is never a resolution candidate, unchanged from ADR-0004.

**Remove value** (`PATCH {"value": null}`, including the suppression form `{"fallback": "none", "value": null}`): one transaction — `UPDATE row SET value_id = NULL, status = 4, value_fp = NULL, fp_key_id = NULL, version = version + 1, <metadata keys>` plus `INSERT gc(old_id, reason = removed)`. No backend call happens before this commits: resolution stops serving the old value at the instant of the commit, not at the instant of a later backend delete. The backend delete of `old_id` follows, best-effort, exactly like step 5 of a write — the same immediate, in-line cleanup path, not a wait for the periodic job.

**Delete record** (`DELETE /credentials/{ref}`): one transaction — `DELETE` the row and, if it held a value, `INSERT gc(value_id, reason = removed)`. The name is released at that instant. A successor `PUT` under the same reference mints its own fresh `value_id` and writes under a different backend key, so the predecessor's lagging backend delete — whenever it eventually runs — can never touch it.

**Expiry**: `expires_at` is still checked on every read, exactly as today — an expired credential stops resolving at read time regardless of when anything else runs. Removing the row itself is not part of any read or write path; it is the periodic maintenance job's other pass, run alongside the gc drain: every `active` row with `expires_at <= now()` is deleted and its version enqueued for collection, one transaction per row, in bounded batches. There is no `deprovisioning` interval to wait through, and — because nothing but the job removes an expired row — the row, and the reference it holds, can sit `active`-but-expired for up to one job period; a create-only `PUT` (`If-None-Match: *`) against it is 409 until the job clears it or the owner acts directly with `PUT If-Match` or `PATCH`.

### The gc table is the intent log

The `pending` reason exists because a crash between step 2 and step 4 has to be discoverable without scanning the backend — and the backend cannot be scanned; it offers no list (D5). A row keyed by `new_id` and carrying `tenant_id`, written before the backend call, is the only durable evidence those bytes were ever meant to exist. Without it, a crash before commit would leave silent, unaccounted-for garbage no periodic job could ever target.

There is no resident process watching this table (D7). A **periodic maintenance job** — an admin entrypoint of the gear binary (`credstore gc`), invoked by the platform scheduler or a Kubernetes CronJob on an operator-chosen cadence, daily by default, weekly acceptable — runs two independent passes against it in bounded batches, until nothing is left:

- **Gc drain.** Any row with `reason ≠ pending` is real garbage — a superseded, removed, or aborted version nothing points to — and is dropped: `plugin.delete` (`NotFound` counts as success) then `DELETE gc`.
- **Pending reclaim.** Any row with `reason = pending` older than `gc.pending_max_age_secs` (default 3600) is a write that never finished within a generous margin. If no row's `value_id` currently equals it, the write never committed — `plugin.delete` + `DELETE gc`. If a row does reference it, only `DELETE gc` runs and the backend entry is never touched. By protocol that branch is unreachable — `DELETE gc(new_id)` sits inside the same step-4 transaction as the pointer switch, so a committed pointer never leaves its intent behind — and it is kept anyway, as the defensive rule that guarantees the job never deletes bytes a live row points at, whatever a bug, a manual repair or a restored backup has done to the table.

There is no grace window any more, and none is needed. Step 5 of a write is the cleanup path for the overwhelmingly common case: it deletes the superseded version and its gc row itself, immediately after the commit, so most gc rows never survive to be seen by a job run at all. With a daily-or-weekly cadence the job cannot race that in-line cleanup in any way that matters: a duplicate `plugin.delete` the job issues against a version step 5 already removed is harmless (`NotFound` is success), and a `DELETE gc` issued after step 5 already removed the row is simply a no-op. What the job's gc drain collects is only what step 5 failed to remove, or what a crash left behind before step 5 ever ran — real garbage every time, never a race with an in-flight cleanup. A reader that resolved the old `value_id` a moment before the pointer moved is unaffected either way: the read protocol's retry-once rule absorbs exactly that case, independently of when the job runs.

### Statuses: two, not four

`CHECK (status IN (2, 4))` admits only `active` (2) and `declared` (4); `provisioning` (1) and `deprovisioning` (3) are retired, their codes reserved and never reassigned, so a stray reference to either in an old dashboard or log line stays unambiguous rather than being silently repurposed. Both retired codes named a step of a saga that no longer exists: `provisioning` meant "the row exists, the backend write may not have landed"; `deprovisioning` meant "the row is gone from resolution but still holds the name". Under the protocol above, a row is only ever fully consistent — it points at a version that exists, or it points at nothing — or it is not yet visible at all: step 4 is the one transaction that makes it consistent, and nothing observable precedes that commit. There is no third state left to name. `declared` keeps its ADR-0004 meaning verbatim: a row with no value, `value_id IS NULL`, reached by `PATCH {"value": null}`.

### What the fence still does

`value_fp = HMAC(fence_key, value)` is stamped in step 4 of the write and verified on every read, unchanged in mechanism from ADR-0003. Its role narrows. Under the saga, a mismatch conflated two causes into one signal: a torn write (the row's step and the backend's step served different writers) or genuine out-of-band tampering or corruption. Under immutable versions the first cause cannot occur — a row never points at a `value_id` until the bytes under that exact id are already fully written, because step 3 precedes step 4 — so a mismatch now means only the second: the backend entry the row currently points to was altered or corrupted outside the API, after the fact.

Recovery changes with the diagnosis. The saga's healing path — an `If-Match: *` PUT of the same bytes, deliberately re-injecting a value the caller already knew, to overwrite the same key and clear the poison — is withdrawn, because there is no "same key" left to re-inject into: a value under a fresh `value_id` is simply a new version, and any ordinary write already produces one. A fingerprint exists for every value; a value-less row has none — the nullability of `value_fp` and `fp_key_id` is tied to the nullability of `value_id`, not stated on its own: `CHECK ((value_id IS NULL) = (value_fp IS NULL) AND (value_id IS NULL) = (fp_key_id IS NULL))`, named `ck_credstore_fp_with_value`. What is withdrawn is the *other* case the shipped schema allowed — a row with a value but no fingerprint (`value_id IS NOT NULL AND value_fp IS NULL`), out-of-band seeding served on trust and backfilled later — because a value now enters the store only through a write that stamps the fence in the same transaction that moves the pointer, so that case can no longer arise. The fence key itself and the split-knowledge property (fingerprints in the DB, key with the values) are unchanged; only its reserved backend entry's address moves to fit the new key shape — see [Backend key shape](#backend-key-shape).

### Preconditions: one ordering

The two overwrite orderings ADR-0001 through ADR-0003 accumulated — CAS the row then write the backend (guarded), or write the backend then CAS the row (`If-Match: *`) — collapse into one: backend first, under a fresh id, then the row transaction, regardless of which precondition the request carries. There is exactly one code path from step 2 through step 5 above; the precondition only decides what step 4's CAS checks. `If-Match: *` is now ordinary last-writer-wins with no special ordering and no healing role — it means exactly what RFC 9110 says it means, answering ADR-0003's "`Exists` writers still race crosswise" amendment by construction rather than by detection: two `If-Match: *` writers still race, but the race now resolves into two intact versions and one pointer, never a mismatched pair.

### Name retention is no longer needed

ADR-0002's `deprovisioning` status existed for one reason: the backend key for a reference was deterministic, so a lagging delete of the outgoing value could land after a successor had already written its own value under the identical key — erasing it. Holding the unique name until the backend delete completed made that race impossible by making it impossible for a successor to exist yet. Under immutable versions the backend key includes `value_id`, so an old version and a new one never share a key: `DELETE` releases the row and the name in the same transaction, a successor `PUT` mints its own fresh `value_id`, and the predecessor's lagging backend delete — whenever it eventually runs — deletes a key the successor never touched. The race ADR-0002 was built to prevent has no precondition left to occur under. Deletion stays exactly as atomic for a reader as it always was — one status filter, no read/delete race — for a smaller reason than before: there is no lifecycle status left to hide behind, because nothing needs hiding.

### Backend key shape

A plugin composes the path `tenant_id/value_id` — no `reference`, no key-class. `value_id`, a UUID v4, is unique across the whole store, so the row alone knows which value belongs to which reference and which sharing class; the backend needs neither to do its job. The plugin SPI drops both accordingly: `get(ctx, tenant_id, value_id) → Option<SecretValue>`, `put(ctx, tenant_id, value_id, value)`, `delete(ctx, tenant_id, value_id)` — no `SecretRef`, no `owner_id: Option<&OwnerId>`. A plugin learns nothing about references, owners, or sharing; it is handed a tenant and an opaque id and nothing more.

The `tenant_id` prefix is kept anyway, so a backend stays partitioned per tenant — a plugin that can list a prefix may still wipe a tenant wholesale on offboarding, but no list capability is required for this scheme to work (D5). Every write lands under a key no prior or future write ever reuses, so "overwrite" is not an operation the plugin contract needs to support. The key is *shorter* than the shipped one, not longer: `tenant_id/value_id` against `tenant/reference/class`; it is exactly as opaque as a UUID v4 is by construction — unpredictable, unguessable, not derivable from the reference — which is the cost this ADR accepts (see Consequences): an operator can no longer map a raw backend entry back to a reference, a sharing class, or a point in time without the row that currently names it as `value_id`.

The fence key's own reserved backend entry ([What the fence still does](#what-the-fence-still-does)) moves with the scheme: it lives under the nil tenant and a fixed, SDK-declared `value_id` constant, `FENCE_KEY_VALUE_ID` — the UUID v5 of the name `cfs-internal-fence-key` — so no metadata row ever points at it, and a random v4 `value_id` can never collide with it.

One consequence of dropping `reference` and key-class from the backend key is worth stating on its own: private and tenant/shared credentials that share one reference no longer need distinct key classes to keep from colliding in the backend, because distinct `value_id`s already do that. The coexistence rule itself — the partial unique indexes on `credstore_secrets` that let a private row and a tenant/shared row share a reference — is unchanged; only the backend's need to encode the class is gone.

Plugins need no list capability under this scheme and gain none: reconciling a version nobody's row points to any more is answered entirely by `credstore_value_gc`, on the credstore side, which is what keeps the plugin contract at exactly three methods (D5).

### The pattern, and why the reaper goes with the saga

None of this is novel, and naming the pattern is what justifies removing the reaper rather than shrinking it. The design is **immutable versions with an atomic pointer switch**, assembled from four well-known pieces:

- **Immutable versioned secrets.** Every managed secret store of note models a secret as an ordered set of immutable versions plus a pointer to the current one: HashiCorp Vault KV v2 keeps numbered versions and never overwrites in place; Google Cloud Secret Manager's `SecretVersion` payloads are immutable and the secret resource points at `latest`; AWS Secrets Manager attaches staging labels (`AWSCURRENT`, `AWSPREVIOUS`) to immutable version ids; Azure Key Vault addresses every secret version by its own identifier. credstore's `value_id` is the same idea, with the version pointer living in the metadata row rather than inside the backend — which is what lets any dumb kv store (D5) participate.
- **Copy-on-write, a.k.a. shadow paging.** Writing the new bytes beside the old ones and publishing them with a single atomic pointer move is shadow paging (Lorie, 1977) — the mechanism that gives copy-on-write engines such as ZFS and LMDB crash consistency without a redo log: the only mutable thing is the pointer, and the pointer changes in one atomic step, so no reader can observe a half-written page. Here the "page" is a backend entry and the "root pointer" is `credstore_secrets.value_id`, moved inside one database transaction.
- **A content store with reachability-based garbage collection — Git's model.** Git objects are immutable and addressed by id; refs point at them; an object no ref reaches is garbage that `git gc --prune` removes whenever it runs, and nothing about the repository's correctness depends on when that is. `credstore_value_gc` plus the maintenance job is exactly this: an unreferenced version is garbage, and collecting it is hygiene, not repair.
- **An intent record instead of a saga.** The `pending` row written before the backend call is the transactional-outbox idea — a durable record of intent kept in the same database as the state it concerns — reduced to a single row, because the only thing that ever needs replaying is the deletion of bytes nobody points at.

Why this *removes* the reaper instead of reshaping it: a saga — the shipped model — has intermediate states (`provisioning`, `deprovisioning`, a torn overwrite) that are **wrong until repaired**, and the reaper was the component whose job was to notice and repair them on a timer, so correctness depended on it running. Under immutable versions there is no intermediate state: a row either points at fully written bytes or at nothing, there is nothing to repair, and no component's lateness can leave a reference wedged or a read closed. What remains is unreachable garbage, and garbage collection is the textbook case for a lazy, periodic, correctness-free job — the `git gc` shape — not for a resident loop inside the service.

References: [Vault KV secrets engine v2](https://developer.hashicorp.com/vault/docs/secrets/kv/kv-v2) · [Google Cloud Secret Manager overview (secret versions)](https://cloud.google.com/secret-manager/docs/overview) · [AWS Secrets Manager: what's in a secret (versions and staging labels)](https://docs.aws.amazon.com/secretsmanager/latest/userguide/whats-in-a-secret.html) · [Azure Key Vault: keys, secrets and certificates (object identifiers, versions)](https://learn.microsoft.com/en-us/azure/key-vault/general/about-keys-secrets-certificates) · [Shadow paging](https://en.wikipedia.org/wiki/Shadow_paging) — R. A. Lorie, *Physical integrity in a large segmented database*, ACM TODS 2(1), 1977 · [Copy-on-write](https://en.wikipedia.org/wiki/Copy-on-write) · [Git Internals — Git Objects](https://git-scm.com/book/en/v2/Git-Internals-Git-Objects) · [`git gc`](https://git-scm.com/docs/git-gc) · [Transactional outbox](https://microservices.io/patterns/data/transactional-outbox.html) · [Saga](https://microservices.io/patterns/data/saga.html).

### Consequences

- Good: a row can never point at bytes that are not the bytes it describes — D1 is now structural, not detected.
- Good: a failure between the stores costs at most an orphaned version the writer's own cleanup, or failing that the periodic maintenance job, collects; never a torn read, a wedged name, or a wrong value served (D2).
- Good: one write ordering, one code path, whichever precondition a request carries (D3).
- Good: two statuses, no name retention (D4).
- Good: the plugin contract stays three methods, no list, no transactions, no `SecretRef` or `owner_id` (D5); a backend with its own native versioning (O3) satisfies this contract trivially by treating `value_id` as an opaque key component.
- Good: a read stays one SQL query plus one backend read on the common path; the one added cost — a retry on a live pointer switch — is rare and bounded to one extra round trip (D6).
- Good: no resident background loop; hygiene runs as a periodic job on a cadence an operator picks, and no correctness property depends on when it runs (D7); no in-process timer also means no per-replica sweep contention and one fewer failure mode inside the gear.
- Cost: the backend accumulates garbage between a write and the writer's own cleanup or the maintenance job's next run — bounded by the job's cadence and, for a crashed write, by `gc.pending_max_age_secs` — but real; a reference under heavy rotation churns more backend storage than in-place overwrite ever did, and an unreferenced entry can survive up to one job period when the writer's own step-5 cleanup fails or the write crashes before reaching it.
- Cost: one extra small DB write per value write — the intent row — before the value is even sent to the backend.
- Cost: the key is opaque — `tenant_id/value_id` is in fact shorter than the shipped `tenant/reference/class` key, not longer, since `reference` and the key-class both drop out — but an operator can no longer predict or reconstruct a value's backend path from the reference alone, only recover it from the row that currently names its `value_id`.
- Cost: out-of-band seeding is gone — a value enters the store only through this protocol, closing the bootstrap shortcut ADR-0003 allowed before any deployment used it.
- Cost: an expired credential stays in the catalogue, holding its reference, until the periodic job removes it or the owner acts directly (`PUT If-Match`/`PATCH`); a create-only `PUT` against an expired reference is 409 until then.

### Confirmation

- E2E: a write killed between step 3 and step 4 leaves the prior value serving unchanged and a `pending` gc row for the orphan; a maintenance-job run, after `gc.pending_max_age_secs` has elapsed with no row referencing it, deletes the backend entry and the gc row.
- E2E: a write that loses its CAS in step 4 leaves the winner's value serving, the loser's bytes recorded `aborted`; if the writer's own best-effort cleanup does not collect it, the next maintenance-job run does.
- E2E: two concurrent `If-Match: *` writers on one reference both receive success; the row ends pointing at whichever committed last; the other's version is enqueued `superseded` and collected by the writer's own step-5 cleanup or, failing that, by the next maintenance-job run.
- E2E: a read landing between a pointer switch and the old version's deletion retries once against the row's current `value_id` and succeeds; only a second consecutive `NotFound` against a still-referenced `value_id` is reported as an error.
- E2E: `DELETE /credentials/{ref}` immediately followed by `PUT /credentials/{ref}` under the same reference succeeds without waiting — no 409, no retry — and the predecessor's version is collected by a maintenance-job run independently of the new one's existence.
- E2E: a maintenance-job run following a torn write — a crash between step 3 and step 4, or a failed step 5 — deletes the orphaned version and its gc row.
- E2E: a maintenance-job run deletes an `active` row past its `expires_at` and enqueues its version for collection, one transaction per row.
- E2E: a maintenance-job run never deletes a version a row still points to, whatever state the gc table is in — the pending-reclaim rule's defensive branch holds even against a manually repaired or restored table.
- E2E: running the maintenance job twice in immediate succession is a no-op the second time — nothing left to expire, drain, or reclaim, and no metric moves.
- Contract test: `PATCH {"value": null}` calls neither `plugin.delete` nor `plugin.put` before its transaction commits; only after.
- Contract: `credstore_secrets.status` never holds `1` or `3` — enforced by the migration's `CHECK`, not only by application logic.
- Contract: `value_fp` and `fp_key_id` are non-`NULL` on every row for which `value_id IS NOT NULL`, and `NULL` on every row for which it is not — enforced by `ck_credstore_fp_with_value`, never only by application logic.
- Contract: no timer task exists in the gear process — hygiene runs only when the `credstore gc` entrypoint is invoked, by a scheduler or an operator, never on an in-process interval.
- E2E: a backend entry corrupted out of band fails its next read closed (fence mismatch, the canonical 404); a subsequent write recovers it by writing a fresh `value_id`, never by re-`PUT`ting the same bytes under the old one — there is no "same key" left to re-`PUT` into.

## Backward Compatibility by Mode

Compatibility is recorded here, not optimized for.

| Surface / mode | Compatible with what ships today | Detail |
|---|---|---|
| Plugin SPI | no — signature change | `get`/`put`/`delete` become `get(ctx, tenant_id, value_id) → Option<SecretValue>`, `put(ctx, tenant_id, value_id, value)`, `delete(ctx, tenant_id, value_id)` — the shipped `(tenant_id, key, owner_id)` triple is replaced by `(tenant_id, value_id)`; `reference` and key-class drop out entirely, and so does `owner_id`. Every plugin implementation — the in-memory static plugin, any future vault plugin — updates its trait impl. No new capability: still `get`/`put`/`delete`, no list, no transactions; the plugin now knows strictly less than before, not more. |
| Statuses | no — two codes retired | `provisioning` (1) and `deprovisioning` (3) are retired, codes reserved and never reused; `CHECK (status IN (2, 4))` replaces the four-valued check. Neither code was ever surfaced by an API response (ADR-0004, "Two representations"), so no external contract references them. |
| Out-of-band seeding | no — withdrawn | A row with a value but no fingerprint (`value_id IS NOT NULL AND value_fp IS NULL`), served on trust, can no longer exist — `ck_credstore_fp_with_value` forbids it. A value enters the store only through the API. An operator who seeded values out of band for bootstrap or migration now writes them through `PUT`/`PATCH`. |
| Fence (ADR-0003) | yes — same mechanism, narrower role | `value_fp`/`fp_key_id` are stamped and verified exactly as before; a fingerprint exists for every value and a value-less row has none (`ck_credstore_fp_with_value` ties their nullability to `value_id`'s). The saga-healing role — an `If-Match: *` re-put as the fix for a poisoned row — is withdrawn; recovery is a new write under a new `value_id`. |
| Reaper | no — withdrawn | The in-process reaper loop — its resident sweep, its `reaper.*` config (`tick_secs`, `provisioning_timeout_secs`, `deprovisioning_timeout_secs`) and its metrics (`provisioning_rollback`, `provisioning_reaped`, `deprovisioning_reaped`) — is removed outright, not renamed. A periodic maintenance job replaces it: an admin entrypoint (`credstore gc`) run by an operator-chosen scheduler, taking `gc.pending_max_age_secs` (default 3600) and `gc.batch_size` (default 256), and emitting `gc_deleted`, `gc_pending_reclaimed`, `expired_deleted` plus a run outcome/duration. |
| REST / SDK surface | yes — unchanged | This ADR sits below [ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md)'s API: every address, verb, precondition and status code it defines is unaffected. A consumer observes no difference except that recovering a corrupted backend entry is now an ordinary write, not a special healing re-put. |
| [ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md) | yes — amended, not superseded | The stateful-gear decision (metadata in the gear's table, values in a plugin) is unchanged and is what makes the pointer possible; only the saga it hosted is replaced. |
| [ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md) | no — superseded | The deprovisioning saga and its name retention are replaced outright; see the amendment note on that ADR. |
| [ADR-0003](0003-cpt-cf-credstore-adr-value-fingerprint-fence.md) | yes — amended | The fence mechanism ships unchanged; its saga role is withdrawn, along with out-of-band seeding. |

## Impact Analysis by Domain

### Security

- **Assets and adversary unchanged from ADR-0004** — the plaintext credential, an over-granted or compromised internal principal. This ADR changes write mechanics beneath that model, not the model itself.
- The intent row (`credstore_value_gc`, `reason = pending`) carries only `tenant_id` and the fresh `value_id` — no `reference`, no `owner_id`, no secret bytes, no fingerprint; an attacker with read access to that table learns only "a write happened in this tenant around this time", with no reference named at all — less than the write audit trail already discloses (ADR-0004, Operations).
- The ABA-shaped race ADR-0002 was built to close has no surface left to attack: there is no interval during which a lagging delete could target a live successor's value, because the two never share a key.
- Corrupted-entry recovery is now an ordinary write, indistinguishable in the audit log from a routine rotation — no special "heal" verb for an attacker to target, but an operator investigating tampering now relies on the `fence_verify{outcome="mismatch"}` metric (ADR-0003) rather than an API-visible recovery action.

### Authentication and authorization

- Unchanged. This ADR sits entirely below ADR-0004's action surface (`read`, `write`, `read_secret`, `write_secret`, `list`, `delete`); no new action, no new principal kind, no added or removed PDP evaluation.
- The injector role's contract (ADR-0004 D4, the roles table's "Injector" row) becomes exception-free: "writes and rotates, guarded or `If-Match: *`" is the whole story, with no separate healing clause, since recovery is the same write path as rotation.

### Integration and contract

- Breaking for plugin authors only: `CredStorePluginClientV1`'s `get`/`put`/`delete` change shape — `value_id` replaces `reference` and `owner_id`, so a call carries `(tenant_id, value_id)` rather than `(tenant_id, reference, owner_id)`. The in-memory static plugin and any future vault plugin update their trait impl; no HTTP or SDK consumer of the credstore gear observes any change, since this ADR is entirely beneath ADR-0004's surface.
- No REST or SDK contract addition or removal.

### Operations

- The in-process reaper is withdrawn: its resident sweep, its `reaper.*` config (`tick_secs`, `provisioning_timeout_secs`, `deprovisioning_timeout_secs`) and its metrics (`provisioning_rollback`, `provisioning_reaped`, `deprovisioning_reaped`) are removed outright, not renamed. A periodic maintenance job (`credstore gc`) replaces it, configured by `gc.pending_max_age_secs` and `gc.batch_size` and invoked by an operator-chosen scheduler — the platform scheduler or a Kubernetes CronJob, out of this ADR's scope; dashboards and alerts built on the old reaper names are retired at rollout, not migrated.
- New runbook signal: the job has not run, or `gc_pending_reclaimed` climbing on every run — the first means hygiene has stopped happening at all, the second means writes are crashing or timing out before commit, not a normal pattern; a sustained climb in backend storage with `gc_deleted` and `expired_deleted` flat across runs means the same thing the first signal does.
- No new infrastructure the gear runs itself: hygiene moves out of the gear's own process entirely, onto whatever the platform already uses to run a scheduled job; provisioning that schedule is out of this ADR's scope.

### Performance

- One extra small, indexed DB write per value write (the intent insert), plus one extra small DB write/delete per successful write (the gc bookkeeping in step 4) — both single-row, single-index, off the read path.
- The read path is unchanged in the common case: one SQL query, one backend read, one fence check. The uncommon case — a read racing a pointer switch — costs one additional row read and one additional backend read, bounded to one retry.
- Backend storage and, for a network-backed plugin, backend request volume both grow relative to in-place overwrite, since every write is a new backend entry rather than a mutation of an existing one; bounded by the maintenance job's cadence (Consequences).

### Reliability

- Every write failure costs at most garbage the writer's own step-5 cleanup removes or, failing that, the periodic maintenance job removes; no failure mode leaves a row pointing at missing bytes, closes a read that should succeed, or wedges a name against recreation (Confirmation).
- The job is idempotent and concurrency-safe: its passes — the expired-row sweep, the gc drain, and pending-reclaim — check state before acting, so running the job twice, or two instances at once, against the same state produces the same outcome.
- Removing the deprovisioning saga removes its own incident class along with it — a row wedged `deprovisioning` past its timeout, needing manual intervention — since there is no multi-step lifecycle state left for a crash to interrupt midway.

### Data

- New column `credstore_secrets.value_id UUID NULL`; new unique partial index `uq_credstore_value_id ON credstore_secrets (value_id) WHERE value_id IS NOT NULL`; `CHECK (status IN (2, 4))` replaces the four-valued check; new `CHECK ((value_id IS NULL) = (value_fp IS NULL) AND (value_id IS NULL) = (fp_key_id IS NULL))` (`ck_credstore_fp_with_value`) ties the fingerprint's presence to the value's.
- New table `credstore_value_gc`, indexed on `enqueued_at` for the periodic maintenance job's age-based passes.
- No new personal data; the gc table stores only the identifier categories the metadata row already does — tenant and value id, no `reference`, no owner — never value bytes or fingerprints.

### Maintainability and evolution

- Two statuses instead of four, one write ordering instead of two, and a periodic maintenance job with two orthogonal, independently testable duties in place of a resident reaper completing a lifecycle saga — the state space a future contributor holds in their head shrinks on every axis D1–D4 named.
- The plugin contract trades two parameters for one — `reference` and `owner_id` drop out, `value_id` replaces them — and gains no new capability; a plugin author's mental model, "a dumb per-tenant key-value store" (ADR-0001), is unchanged, keyed by an opaque id instead of a reference.
- A future backend-native-versioning plugin (O3, Revisit Triggers) is additive: nothing here forecloses a plugin from using its own backend's version history internally, as long as it honors the `get`/`put`/`delete`-by-`value_id` contract at its boundary.

### Compliance

- Supports `nfr-confidentiality` exactly as ADR-0004 already established; this ADR changes nothing about what leaves the gear or how a disclosure is audited.
- Strengthens provenance: every version ever written has exactly one intent row explaining why, and, once superseded or removed, exactly one record of when and why it stopped being current — a stronger trail of "which bytes were live when" than in-place overwrite ever offered, since overwrite destroyed the previous bytes' backend residency the instant it landed.

### Not applicable

- **Usability:** no end-user interface; unchanged from ADR-0004.
- **Session management:** the gear holds no sessions.
- **Data migration:** greenfield — no production rows exist, so `m0002` ships as a plain additive migration. If rows existed, a one-off job would mint a `value_id` per `active` row, copy its backend entry to the versioned key, and set the pointer, before `ck_credstore_fp_with_value` is enforced.
- **Penetration testing as a gate:** unchanged — the security confirmation is the contract and E2E invariants above; scheduled testing is a platform activity outside this decision.

## Revisit Triggers

- A plugin wants to use its backend's own native versioning (Vault KV v2, GCP Secret Manager `SecretVersion`) directly rather than through the `value_id`-keyed contract — see O3; today that stays an implementation detail behind the three-method SPI, and promoting it to the contract is a revisit.
- Garbage or expired-row lifetime of one job period becomes a problem for a high-rotation reference or a storage-priced backend, arguing for a shorter cadence, or for an eager in-request drain of a bounded number of gc rows on each write instead of waiting for the next scheduled run.
- Out-of-band seeding is wanted back for a bootstrap or migration path; reintroducing it needs a narrower story than ADR-0003's, since `ck_credstore_fp_with_value` and "a value enters only through the API" are now load-bearing elsewhere in this ADR.
- The gc table's reconciliation guarantee — finding versions nobody points to — rests entirely on the gc table itself, since plugins have no list capability; a plugin that can list cheaply could support periodic reconciliation as a second line of defense against a lost gc row, which today has no backstop beyond the write and the gc insert being one transaction.

## Pros and Cons of the Options

### O1: in-place overwrite, saga, fence (shipped)

- Good: ships today; no migration, no plugin SPI change, no new table.
- Good: the backend never accumulates garbage — one key per reference-class, always current.
- Bad: a torn write between `plugin.put` and the row commit serves 404 until a caller happens to re-put (D1, D2).
- Bad: a stuck `provisioning` row wedges creation under that reference until the reaper's timeout elapses (D2, D4).
- Bad: `deprovisioning` holds the name hostage to a race that exists only because the key scheme reuses the backend key — the entire mechanism ADR-0002 built exists to guard a hazard this option's key scheme creates (D4).
- Bad: two overwrite orderings, CAS-then-backend for guarded writes and backend-then-CAS for `If-Match: *`, is two code paths and two mental models for one operation (D3).
- Rejected as the ongoing model, superseded by O2.

### O2: immutable versions, pointer, gc table (CHOSEN)

- Good: satisfies D1 structurally — see Decision Outcome throughout.
- Good: a failure anywhere in the write costs garbage, never a closed read, a wrong value, or a wedged name (D2).
- Good: one write ordering regardless of precondition (D3).
- Good: two statuses, no name retention (D4).
- Good: the plugin stays three dumb methods; the gc table, not plugin-side listing, is what makes reconciliation possible (D5).
- Good: the read path adds nothing in the common case and one bounded retry in the race case (D6).
- Good: no resident background loop — hygiene is a periodic job on a cadence an operator picks, and no correctness property depends on when it runs (D7).
- Bad: backend garbage between a write and the periodic job's next run, an extra small DB write per value write, a backend key that is opaque (shorter than today's, not longer, but unpredictable), the loss of out-of-band seeding, and an expired credential that lingers in the catalogue until the job clears it — all named plainly in Consequences.

### O3: backend-native versioning

- Good: a backend with its own version history (Vault KV v2's numbered versions and `delete_version_after`, GCP Secret Manager's `SecretVersion` resources) needs no `value_id` parameter at all — the backend already is the intent log and the gc mechanism.
- Bad, as the *contract*: the plugin SPI promises that any dumb kv store qualifies as a backend (ADR-0001); a contract built around native versioning would exclude every backend without it, including the shipped in-memory static plugin.
- Rejected as the contract for exactly that reason; noted as an implementation a Vault-backed plugin may choose underneath the `value_id`-keyed SPI this ADR defines — nothing here prevents a plugin from mapping `value_id` onto its backend's own version number instead of a raw key suffix.

### O4: two-phase commit, outbox

- Good: a textbook answer to coordinating a write across two stores without a shared transaction — a prepare/commit protocol or an outbox table would also make the row/backend disagreement structurally bounded.
- Bad: heavier than the guarantee needs. The intent row this ADR uses already gives a crash a single, cheap, indexed place to be discovered and reconciled; a full 2PC coordinator or an outbox-with-dispatcher adds a component, a failure mode of its own (a stuck coordinator, a lagging consumer), and an operational surface the platform's cluster primitives explicitly steer away from for external side effects (ADR-0003's citation of cluster ADR-002: no remote I/O inside a coordinated critical section).
- Rejected: the intent-row/gc-table pair is the outbox pattern's essential idea — a durable record of intent, drained asynchronously — at the minimum weight this problem needs, with no second coordination protocol layered on top.

## Traceability

- Builds on [ADR-0001](0001-cpt-cf-credstore-adr-stateful-gear.md): the metadata table and the pointer it now carries live in the store that ADR established; the stateful-gear decision is what makes a pointer possible at all.
- Supersedes [ADR-0002](0002-cpt-cf-credstore-adr-deprovisioning-saga.md): the deprovisioning saga and its name retention are replaced by a one-transaction delete plus garbage collection.
- Amends [ADR-0003](0003-cpt-cf-credstore-adr-value-fingerprint-fence.md): the fence mechanism is unchanged; its saga-healing role is withdrawn along with out-of-band seeding.
- Defines the write protocol [ADR-0004](0004-cpt-cf-credstore-adr-secret-value-exposure.md) builds its surface on, including the crash-safety of `PATCH {"value": null}`.
- Leaves [ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md) untouched: the collection read resolves through the same row and the same `value_id` pointer, with no change to its authorization or reduction logic.
- Requirements: `cpt-cf-credstore-fr-put-secret`, `cpt-cf-credstore-fr-delete-secret`, `cpt-cf-credstore-fr-deprovisioning`, `cpt-cf-credstore-fr-optimistic-concurrency`, `cpt-cf-credstore-fr-write-secret`, `cpt-cf-credstore-fr-write-credential-record`, `cpt-cf-credstore-nfr-confidentiality`.
- New requirement: `cpt-cf-credstore-fr-immutable-value-versions`, added to the PRD §5.8 alongside this project's other proposed requirements.
