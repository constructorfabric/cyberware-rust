Created:  2026-07-08 by Constructor Tech
Updated:  2026-07-08 by Constructor Tech
# Feature: Ownership Transfer

- [ ] `p2` - **ID**: `cpt-cf-file-storage-featstatus-ownership-transfer-implemented`

> The endpoint, atomic owner swap, audit row, file event, and usage-delta
> reporting are fully tested. Target-owner validation is partial: the only
> guard is rejecting the nil UUID; `new_owner_id` is not verified to name a
> real principal. See [§1.2's caveat](#12-purpose) and the acceptance
> criteria in §6.



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Transfer File Ownership](#transfer-file-ownership)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Rebalance Usage Ledger After Transfer](#rebalance-usage-ledger-after-transfer)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Transfer Endpoint with Atomic Owner Swap](#transfer-endpoint-with-atomic-owner-swap)
  - [Target-Owner Validation (PARTIAL)](#target-owner-validation-partial)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

- [ ] `p2` - `cpt-cf-file-storage-feature-ownership-transfer`

### 1.1 Overview

`POST /files/{id}/transfer` atomically replaces a file's `owner_kind` +
`owner_id`, in the same DB transaction as (a) a `TransferOwnership` audit row
and (b) a `file.owner_transferred` file event. After the transfer commits, the
service also fires a pair of fire-and-forget usage-delta reports: debit the
old owner's byte/file counters, credit the new owner's.

**Traces to**: `cpt-cf-file-storage-fr-ownership-transfer`, `cpt-cf-file-storage-fr-audit-trail`,
`cpt-cf-file-storage-fr-file-events`, `cpt-cf-file-storage-fr-usage-reporting`

### 1.2 Purpose

Let a file's owner change (e.g. a user leaving a team, or a file being
reassigned to a service account) without recreating the file, losing its
`file_id`/version history/custom metadata, or breaking any existing
references to it. Because the swap, the audit row, and the file event are all
written in one transaction, a caller can never observe a state where the
owner changed but no audit trail exists for it, or vice versa.

> **Caveat: target-owner validation is partial.** `transfer_ownership`
> rejects `new_owner_id` only when it is the **nil UUID** — an obviously
> malformed sentinel value, checked with `Uuid::is_nil()`
> (`src/domain/service/write.rs::transfer_ownership`). It does **not** verify
> that `new_owner_id` names a real, existing principal, nor that the principal
> is actually a member of the caller's tenant. `cf-gears-file-storage` has no
> account-management SDK wired in and no principal directory of its own, so it
> has no data source to check against — full existence/membership validation is
> **blocked on an account-management SDK dependency** that does not exist yet.
> Note, however, that a *cross-tenant* transfer is already structurally
> impossible through this endpoint regardless of that gap: the updated row's
> `tenant_id` always comes from the existing file (`self.store.require_file`
> scoped to `Self::tenant_scope(ctx)`, i.e. `ctx.subject_tenant_id()`), never
> from the request body, so `new_owner_id` can only ever be recorded under the
> caller's own tenant — it is impossible to use this endpoint to move a file
> into a different tenant, only to (mis)attribute it to an arbitrary UUID
> within the caller's own tenant. Whether ownership transfer should also
> require a distinct privileged-transfer grant (rather than reusing the
> ordinary file `WRITE` authorization) is a related, separately open
> admin-scope authorization decision — not resolved by this feature either.

**Requirements**: `cpt-cf-file-storage-fr-ownership-transfer`

**Principles**: none specific to this feature beyond the general audit/event guarantees

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-file-storage-actor-platform-user` | Calls `POST /files/{id}/transfer` on a file they have `WRITE` authorization over; must supply a well-formed (non-nil) `new_owner_id` |
| `cpt-cf-file-storage-actor-cf-gears` | Peer gear / service invoking the same endpoint on behalf of a reassignment workflow it manages |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **DECOMPOSITION**: [DECOMPOSITION.md](../DECOMPOSITION.md)
- **Dependencies**: [Audit Trail](audit-trail.md) (the `TransferOwnership` audit
  row shares that feature's transactional-outbox mechanism); the file-events
  outbox (`cpt-cf-file-storage-fr-file-events`, same undrained-relay caveat);
  usage reporting (`cpt-cf-file-storage-fr-usage-reporting`) for the
  post-commit debit/credit

## 2. Actor Flows (CDSL)

### Transfer File Ownership

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-ownership-transfer`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- The file's `owner_kind`/`owner_id` are atomically replaced; an audit row and
  a `file.owner_transferred` event are recorded in the same transaction; usage
  deltas are reported for the old and new owner; the caller receives the
  updated `File` representation, reflecting the new owner and bumped
  `last_modified_at` — both the `File` row and its custom metadata are
  captured **before** the transfer and the known post-transfer field changes
  applied locally, rather than re-read from the DB afterward, since an
  owner-constrained `AccessScope` captured before the swap would no longer
  match the row under its new owner and a successful transfer would otherwise
  incorrectly surface as `404`

**Error Scenarios**:
- `new_owner_id` is the nil UUID — `400` (`Validation`, field `new_owner_id`)
- `new_owner_kind` is neither `"user"` nor `"app"` — `400` (`Validation`,
  field `new_owner_kind`)
- File not found, or `transfer_ownership_atomic`'s scoped `UPDATE` matches
  zero rows (e.g. concurrent delete) — `404` (`FileNotFound`); **no** audit
  row and **no** file event are written in this case (proven by
  `tests/ownership_test.rs::transfer_ownership_no_row_means_no_audit_and_no_event`)
- Caller lacks `WRITE` authorization on the file — `403`

**Steps**:
1. [x] - `p1` - Client: POST /api/file-storage/v1/files/{id}/transfer with body {new_owner_kind, new_owner_id} - `inst-transfer-request`
2. [x] - `p1` - API: reject `new_owner_id == Uuid::nil()` with `400` before touching the DB (**the only target-owner validation implemented — see the §1.2 caveat**) - `inst-transfer-nil-check`
3. [x] - `p1` - API: parse `new_owner_kind`; reject anything other than `"user"`/`"app"` with `400` - `inst-transfer-kind-parse`
4. [x] - `p1` - Control plane: load the file scoped to the caller's tenant; authorize `WRITE` on `file_id` - `inst-transfer-authz`
5. [x] - `p1` - Control plane: capture the file's custom metadata **before** the transfer, so a caller who loses read access under the new owner still receives accurate metadata in the response - `inst-transfer-capture-meta`

   The `File` row itself is already held from step 4's prefetch; it is *not* re-read after the commit — the response is built by applying the known post-transfer field changes (`owner_kind`, `owner_id`, `last_modified_at`) to that pre-transfer row, avoiding a post-commit re-read under a now-stale, potentially owner-constrained `AccessScope`.
6. [x] - `p1` - Build the `TransferOwnership` audit row and the `file.owner_transferred` file event, both carrying `from_owner_kind`/`from_owner_id`/`to_owner_kind`/`to_owner_id` - `inst-transfer-build-audit-event`
7. [x] - `p1` - DB: `transfer_ownership_atomic` — in one transaction, `UPDATE files SET owner_kind, owner_id` scoped to the tenant + `file_id`, insert the audit row (only if the update matched a row), insert the event row; RETURN whether a row was updated - `inst-transfer-atomic-update`
8. [x] - `p1` - **IF** no row was updated (file not found, or removed by a concurrent delete): RETURN `404 FileNotFound`, no audit row, no event - `inst-transfer-not-found`
9. [x] - `p1` - Compute the file's total available-version bytes; fire (fire-and-forget) a usage-delta debit for the old owner and a credit for the new owner using `cpt-cf-file-storage-algo-ownership-transfer-usage-rebalance` - `inst-transfer-usage-rebalance`
10. [x] - `p1` - RETURN `200` with the updated `File` (+ the pre-captured metadata) - `inst-transfer-return`

## 3. Processes / Business Logic (CDSL)

### Rebalance Usage Ledger After Transfer

- [x] `p2` - **ID**: `cpt-cf-file-storage-algo-ownership-transfer-usage-rebalance`

**Input**: `tenant_id`, `old_owner_id`, `new_owner_id`, the sum of `size` over
every `Available` version of the file

**Output**: two `UsageDelta` reports, dispatched fire-and-forget (`tokio::spawn`,
failures logged but never propagated back to the caller)

**Steps**:
1. [x] - `p1` - List all versions of the file; sum `size` over versions whose status is `Available` (pending/superseded versions do not count) - `inst-rebalance-sum`
2. [x] - `p1` - Report `UsageDelta { tenant_id, owner_id: old_owner_id, bytes_delta: -total_bytes, file_count_delta: -1 }` - `inst-rebalance-debit`
3. [x] - `p1` - Report `UsageDelta { tenant_id, owner_id: new_owner_id, bytes_delta: total_bytes, file_count_delta: 1 }` - `inst-rebalance-credit`
4. [x] - `p1` - Both reports are no-ops when no `UsageReporter` is wired (`self.usage_reporter` is `None`) — this mirrors the rest of the gear's usage-reporting posture (`cpt-cf-file-storage-fr-usage-reporting`), not a gap specific to this feature - `inst-rebalance-noop-if-unwired`

## 4. States (CDSL)

**Not applicable.** A file's `(owner_kind, owner_id)` pair is a plain mutable
attribute updated by direct `UPDATE`, not a modeled state machine with its own
transitions or invalid states — every well-formed `(owner_kind, owner_id)`
pair is a valid target.

## 5. Definitions of Done

### Transfer Endpoint with Atomic Owner Swap

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-ownership-transfer-endpoint`

The system **MUST** implement `POST /api/file-storage/v1/files/{id}/transfer`:
authorize `WRITE` on the file; atomically update `owner_kind`/`owner_id` in
the same transaction as a `TransferOwnership` audit row and a
`file.owner_transferred` event; report usage deltas for the old and new owner
after commit; return the updated `File`. A transfer that matches zero rows
(file not found, or a lost race) leaves no audit row and no event.

**Implements**:
- `cpt-cf-file-storage-flow-ownership-transfer`
- `cpt-cf-file-storage-algo-ownership-transfer-usage-rebalance`

**Touches**:
- API: `POST /api/file-storage/v1/files/{id}/transfer`
- DB Table: `files`
- DB Table: `audit_outbox`
- DB Table: `events_outbox`

### Target-Owner Validation (PARTIAL)

- [ ] `p2` - **ID**: `cpt-cf-file-storage-dod-ownership-transfer-target-validation`

The system **SHOULD** verify that `new_owner_id` names a real, existing
principal within the caller's tenant before committing a transfer. **Only the
nil-UUID rejection is implemented.** Full existence/tenant-membership
validation requires a cross-gear account-management lookup that does not
exist yet (no account-management SDK is wired into
`cf-gears-file-storage`). This DoD line stays explicitly unchecked until that
dependency is available — it is not silently treated as satisfied by the
nil-UUID guard.

**Implements**: (blocked — no account-management SDK dependency to build against yet)

**Touches**:
- API: `POST /api/file-storage/v1/files/{id}/transfer` (request validation only)

## 6. Acceptance Criteria

- [x] `POST /files/{id}/transfer` updates `owner_kind`/`owner_id` on the file row (`tests/ownership_test.rs::transfer_ownership_updates_owner_fields`)
- [x] A `TransferOwnership` audit row is written in the same transaction as the owner update (`::transfer_ownership_leaves_audit_row`)
- [x] A `file.owner_transferred` event is enqueued in `events_outbox` in the same transaction (`::transfer_ownership_enqueues_file_event`)
- [x] Transferring a non-existent file returns `FileNotFound` (`::transfer_ownership_non_existent_file_returns_not_found`)
- [x] A transfer whose scoped `UPDATE` matches zero rows writes **no** audit row and **no** event (`::transfer_ownership_no_row_means_no_audit_and_no_event`)
- [x] `new_owner_id == Uuid::nil()` is rejected with a validation error before any DB write (`::transfer_to_malformed_owner_is_rejected`)
- [x] A well-formed `new_owner_id` under the caller's own tenant succeeds (`::transfer_to_same_tenant_member_succeeds`) — this is also the only kind of transfer the endpoint can perform, since `tenant_id` is never taken from the request
- [x] Usage deltas are reported: the old owner is debited and the new owner is credited by the file's total available-version bytes, and by one `file_count_delta` each
- [ ] `new_owner_id` is validated against a real, existing, same-tenant principal — **PARTIAL**; only the nil-UUID sentinel is rejected today, blocked on an account-management SDK dependency (see the caveat in §1.2 and the DoD in §5)
- [ ] Ownership transfer requires a distinct privileged-transfer authorization grant rather than reusing the file's ordinary `WRITE` grant — **not decided**; a separate, open admin-scope authorization question, out of this feature's current scope
