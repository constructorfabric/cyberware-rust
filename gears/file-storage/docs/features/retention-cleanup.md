Created:  2026-07-08 by Constructor Tech
Updated:  2026-07-08 by Constructor Tech
# Feature: Retention Policies & Cleanup (Orphan Reconciliation)

- [ ] `p2` - **ID**: `cpt-cf-file-storage-featstatus-retention-cleanup-implemented`



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [List Retention Rules](#list-retention-rules)
  - [Create Retention Rule](#create-retention-rule)
  - [Delete Retention Rule](#delete-retention-rule)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Run Sweep Cycle](#run-sweep-cycle)
  - [Sweep Abandoned Pending Versions (Orphan Reconciliation)](#sweep-abandoned-pending-versions-orphan-reconciliation)
  - [Sweep Expired Multipart Sessions](#sweep-expired-multipart-sessions)
  - [Sweep Retention-Policy Expiry](#sweep-retention-policy-expiry)
  - [Validate Retention Rule on Write](#validate-retention-rule-on-write)
- [4. States (CDSL)](#4-states-cdsl)
  - [Multipart Session (owned by multipart-coordinator, driven here on a timer)](#multipart-session-owned-by-multipart-coordinator-driven-here-on-a-timer)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Retention Rule Domain Types and Administration Endpoints](#retention-rule-domain-types-and-administration-endpoints)
  - [Cleanup Engine and Background Sweep Scheduling](#cleanup-engine-and-background-sweep-scheduling)
  - [Live-Multipart-Session Guard](#live-multipart-session-guard)
  - [Semantic Validation on Write](#semantic-validation-on-write)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

- [ ] `p2` - `cpt-cf-file-storage-feature-retention-cleanup`

### 1.1 Overview

Two related P2 capabilities sharing one background engine (`CleanupEngine::run_sweep`, `src/domain/cleanup.rs`):
(1) **retention policies** (`cpt-cf-file-storage-fr-retention-policies`) — tenant/user/file-scoped rules that
auto-expire files by age, inactivity, or a custom-metadata match; and (2) **orphan reconciliation**
(`cpt-cf-file-storage-fr-orphan-reconciliation`) — reclaiming `pending` version rows (and, transitively, permanently
orphaned zero-version `files` rows) that were pre-registered but never finalized, and aborting multipart sessions
whose TTL expired without a `complete`/`abort` call. The sweep also purges expired `idempotency_keys` rows (a
housekeeping task riding the same cycle, not part of either named requirement). One background task per
control-plane instance runs the full sweep on a fixed interval; there is no cross-instance coordination in P2 — see
§4.

### 1.2 Purpose

Two independent problems addressed by one engine because both are "delete files/rows the system decides are no
longer wanted, on a schedule, without a public trigger surface": regulated environments and cost-conscious tenants
need automated lifecycle management (retention), and the control plane's own two-phase upload protocol (pre-register
a `pending` version, then a separate later `bind`/`complete` — not atomic with the initial write) inevitably leaves
abandoned rows behind when a client disappears mid-upload (orphan reconciliation). Both are **best-effort**: one
step's failure is logged and does not abort the rest of the cycle, and every delete is idempotent so redundant
concurrent sweeps across instances are safe, merely wasteful.

**Requirements**: `cpt-cf-file-storage-fr-retention-policies`, `cpt-cf-file-storage-fr-orphan-reconciliation`

**Principles**: `cpt-cf-file-storage-principle-control-no-content`

> **Scope note.** The PRD's `cpt-cf-file-storage-fr-orphan-reconciliation` also describes **backend blob-without-row**
> reconciliation (cross-backend orphan enumeration via a `list_paths` scan) and flagging an `available` version with
> no matching backend object for operator attention. **Neither is implemented in P2** — both require cross-instance
> leader election to run safely (a naive per-instance `list_paths` scan would race and double-report/double-delete
> across replicas) and are explicitly deferred to P3 per `cleanup.rs`'s module doc comment. P2's orphan reconciliation
> is scoped to the two cases enumerated in §3 below: abandoned `pending` versions and expired multipart sessions.

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-file-storage-actor-platform-user` | Defines tenant/user/file-scope retention rules; their files are subject to both retention expiry and orphan reconciliation |
| `cpt-cf-file-storage-actor-cf-gears` | Peer gear / service whose files are equally subject to both sweep behaviors; no distinct role from the platform user here |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5.6 "Retention Policies", "Orphan Reconciliation"
- **Design**: [DESIGN.md](../DESIGN.md)
- **API contract**: [api.md](../api.md) — `GET/POST /retention-rules`, `DELETE /retention-rules/{rule_id}`
- **Dependencies**: [Multipart Upload Coordinator](multipart-coordinator.md)
  (`cpt-cf-file-storage-feature-multipart-coordinator`) — the sweep's expired-multipart step reuses that feature's
  session state machine (`in_progress -> aborted` via TTL, `cpt-cf-file-storage-state-multipart-session`'s third
  transition) and CAS pattern; this feature does not change that state machine, only drives one of its transitions
  on a timer instead of a client call

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor. The sweep cycle itself has **no actor-facing flow** — PRD
§5.6 requires it be a control-plane internal scheduled task, not triggerable from any public API surface; it is
documented as a process in §3 instead.

### List Retention Rules

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-retention-list`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- A caller holding `ADMIN_POLICY` receives every retention rule (any scope) belonging to their tenant
- A non-admin caller receives only the rules they may see: all `Tenant`-scope rules, `User`-scope
  rules that target themselves, and `File`-scope rules whose target file they own (owner compared
  as the `(owner_kind, owner_id)` pair, not `owner_id` alone — `user` and `app` are disjoint owner
  spaces that can share a UUID) — the store query has no owner/target filter, so without this the
  response would leak every other tenant member's retention configuration
- **Known limitations on the `File`-scope branch.** A rule whose target file has since been deleted
  stays invisible to every non-admin caller, including whoever created it: `StoredRetentionRule`
  carries no creator/`subject_id` column, so once the target file is gone there is no stored fact
  left to compare against (an admin can still see it via `ADMIN_POLICY`, or remove it via
  `delete_retention_rule`'s own dangling-target fallback). Separately, a `File`-scope rule can be
  created by anyone holding per-file `WRITE` on the target — not only the file's owner — so a
  subject who created such a rule via delegated `WRITE` on someone else's file will not see it in
  this listing: visibility is gated on being the target file's *owner*, not on having created the
  rule.

**Error Scenarios**:
- Caller lacks `READ` — `403`

**Steps**:
1. [x] - `p1` - Client: GET /api/file-storage/v1/retention-rules - `inst-retention-list-request`
2. [x] - `p1` - Authorize `READ` on `("", None)` - `inst-retention-list-authz`
3. [x] - `p1` - DB: SELECT all retention rules for the caller's tenant - `inst-retention-list-load`
4. [x] - `p1` - Filter the result: pass it through unfiltered if the caller also holds `ADMIN_POLICY`;
   otherwise keep only `Tenant`-scope rules, `User`-scope rules targeting the caller, and
   `File`-scope rules whose target file's `(owner_kind, owner_id)` pair matches the caller —
   dropping every other `File`-scope rule, including ones targeting an already-deleted file
5. [x] - `p1` - RETURN 200 with the list - `inst-retention-list-return`

### Create Retention Rule

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-retention-create`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Rule is created and returned; it becomes eligible for matching on the next sweep cycle

**Error Scenarios**:
- The body specifies none of `age`/`inactivity`/`metadata` — `400` (a rule that can never
  match any file is almost certainly a mistake)
- `age.max_age_days == 0` or `inactivity.inactivity_days == 0` — `400` (would match *every* file in the tenant on
  the very next sweep tick, permanently deleting rows and blobs with no dry-run and no undo)
- `scope ∈ {user, file}` with no `scope_target_id` — `400` (a dead rule that can never resolve to a target)
- `scope = file` and the target file does not exist, or belongs to a different
  tenant, or the caller lacks `WRITE` on it — `404`/`403` (`authorize_retention_scope`'s
  `File` arm resolves the target via `require_file` scoped to the caller's own
  tenant — the same prefetch pattern `read_ops.rs`/`write.rs` use — before
  authorizing, so a foreign tenant's file UUID cannot be distinguished from a
  nonexistent one; both surface identically as `FileNotFound`)
- `scope = user` and the target is a different user, without `ADMIN_POLICY` — `403`
- Caller lacks `ADMIN_POLICY` (tenant scope, no `WRITE` fallback) — `403` (a tenant-scope rule is a
  standing instruction for the background sweep to permanently delete every matching file for every
  subject in the tenant, so ordinary per-file `WRITE` is not enough)

**Steps**:
1. [x] - `p1` - Client: POST /api/file-storage/v1/retention-rules {scope, scope_target_id?, body} - `inst-retention-create-request`
2. [x] - `p1` - Algorithm: `cpt-cf-file-storage-algo-validate-retention-rule` — reject a dead-on-write or immediately-total-expiry body - `inst-retention-create-validate` (runs before authorization: otherwise the same malformed body answers `403` for a non-admin caller and `400` for an admin, since only the admin ever reaches validation)
3. [x] - `p1` - Authorize by scope: `Tenant` → `ADMIN_POLICY` outright, no fallback to `WRITE`; `User` → `ADMIN_POLICY`-first with a `WRITE`-plus-target-match fallback (a missing target is a mismatch, not "no check" — unlike the policy-engine's tenant-scope fallback); `File` → resolve the target file via `require_file`, scoped to the caller's own tenant (a foreign or missing file surfaces as `FileNotFound`, not silently accepted) then require per-file `WRITE` - `inst-retention-create-authz`
4. [x] - `p1` - DB: INSERT the rule row - `inst-retention-create-insert`
5. [x] - `p1` - RETURN 201 with the created rule - `inst-retention-create-return`

### Delete Retention Rule

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-retention-delete`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Rule is deleted
- A `scope = file` rule whose target file has since been deleted (there is no
  FK/cascade between `retention_rules.scope_target_id` and `files.file_id`) is
  still deletable: the caller is re-authorized against a plain tenant-wide
  `WRITE` grant instead of a per-file check that can no longer resolve a
  target, so the orphaned rule does not become permanently stuck and does not
  keep being re-scanned by every sweep

**Error Scenarios**:
- `rule_id` does not exist — `404`
- Caller does not own the rule's scope/target (same rules as create) — `403`
- A foreign-tenant caller's `rule_id` (the rule belongs to a different
  tenant) — `404`, not `403`: step 2's fetch is scoped to the caller's own
  tenant, so a foreign tenant's `rule_id` simply does not resolve there — it
  404s the same way a genuinely nonexistent `rule_id` does, before
  authorization is ever attempted

**Steps**:
1. [x] - `p1` - Client: DELETE /api/file-storage/v1/retention-rules/{rule_id} - `inst-retention-delete-request`
2. [x] - `p1` - DB (fetch-then-reauthorize): SELECT the rule scoped to the caller's own tenant, purely to learn its `(scope, scope_target_id)` — a bare `rule_id` carries no ownership information, so a coarse tenant-wide `DELETE` check alone would let any tenant member delete any other member's rule; scoping this fetch to the caller's own tenant also means a foreign tenant's `rule_id` simply does not resolve, `404`-ing here exactly like a nonexistent one, before authorization is ever consulted - `inst-retention-delete-load`
3. [x] - `p1` - Re-run the same scope-based authorization [Create Retention Rule](#create-retention-rule) uses, against the rule's actual `(scope, scope_target_id)`; for `scope = file`, if the target file no longer exists (`FileNotFound`), fall back to a plain tenant-wide `WRITE` gate — there is no file left to check per-file `WRITE` against, so this is the closest available equivalent, deliberately NOT raised to `ADMIN_POLICY` since a rule whose target file is already gone governs nothing — rather than propagating the 404 — an orphaned file-scope rule must remain deletable - `inst-retention-delete-authz`
4. [x] - `p1` - DB: DELETE the rule row, scoped to the authorized `AccessScope` - `inst-retention-delete-remove`
5. [x] - `p1` - RETURN 204 - `inst-retention-delete-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions with no direct actor interaction; the background sweep loop is the only caller.

### Run Sweep Cycle

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-run-sweep`

**Input**: none (reads current time and the `CleanupConfig.orphan_grace_secs` knob)

**Output**: `SweepResult { abandoned_pending_deleted, abandoned_files_deleted, expired_multipart_aborted,
retention_expired_deleted, idempotency_keys_deleted }`

**Steps**:
1. [x] - `p1` - Step 1: sweep abandoned pending versions (+ any now-permanently-orphaned parent `files` row) —
   `cpt-cf-file-storage-algo-sweep-abandoned-pending` - `inst-sweep-step1`
2. [x] - `p1` - Step 2: sweep expired `in_progress` multipart sessions —
   `cpt-cf-file-storage-algo-sweep-expired-multipart` - `inst-sweep-step2`
3. [x] - `p1` - Step 3: sweep retention-policy expiry across all scopes —
   `cpt-cf-file-storage-algo-sweep-retention-expiry` - `inst-sweep-step3`
4. [x] - `p1` - Step 4: delete expired `idempotency_keys` rows (`expires_at <= now`); `audit_outbox`/`events_outbox`
   rows are deliberately **not** touched here regardless of age, because `published_at` stays `NULL` until the
   Tier-4 `EventBroker` relay exists — an age-based purge would silently drop undelivered events - `inst-sweep-step4`
5. [x] - `p1` - Each step is independently best-effort: a store error is logged at `warn` and contributes `0` to
   that step's tally rather than aborting the remaining steps - `inst-sweep-best-effort`
6. [x] - `p1` - RETURN the accumulated `SweepResult`; the gear also exports these five tallies as metrics counters
   at the point they are logged - `inst-sweep-return`

### Sweep Abandoned Pending Versions (Orphan Reconciliation)

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-sweep-abandoned-pending`

**Input**: `grace_cutoff` (`now - orphan_grace_secs`), `now`

**Output**: `(pending_versions_deleted, orphan_files_deleted)`

**Steps**:
1. [x] - `p1` - DB: list `pending` version rows with `created_at < grace_cutoff`, **excluding** any version that is still the backing version of a live `in_progress` multipart session (`multipart_uploads.expires_at > now`) — see [Live-Multipart-Session Guard](#live-multipart-session-guard) - `inst-sweep-pending-list`
2. [x] - `p1` - FOR EACH candidate: write an `orphan_reconcile` audit row, then delete the version row **status-guarded** (`status = pending` only) -- the same CAS pattern step 2 below uses, so a version a racing `finalize_upload` already flipped to `available` between the list query and this delete is left completely untouched (row, blob, and debit alike) - `inst-sweep-pending-audit-delete`
3. [x] - `p1` - **IF** deleted: debit the reclaimed bytes via the usage reporter (fire-and-forget; `bytes_delta = -size`, `file_count_delta = 0` — `size` is structurally `0` in practice since a version is only ever assigned a nonzero size by `finalize_version`, which a reclaimed-here version never reached) - `inst-sweep-pending-usage`
4. [x] - `p1` - Best-effort: delete the backend blob at the version's `(backend_id, backend_path)` — a failure leaves an unreachable orphan blob, acceptable in P2 - `inst-sweep-pending-blob`
5. [x] - `p1` - **IF** the parent file now has zero versions **AND** `content_id IS NULL` **AND** no `in_progress`, unexpired multipart session still references it (the same guard as step 1, re-checked because a session that has not yet expired could still legitimately have its backing version reclaimed by an unrelated grace-window aging in the *same* sweep pass): delete the `files` row too, transactionally re-verifying both conditions inside the delete so a version inserted in the gap is never lost — write a `file.deleted` event and debit `file_count_delta = -1`, `bytes_delta = 0` - `inst-sweep-pending-orphan-file`
6. [x] - `p1` - RETURN the two counts - `inst-sweep-pending-return`

### Sweep Expired Multipart Sessions

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-sweep-expired-multipart`

**Input**: `now`

**Output**: count of sessions aborted

**Steps**:
1. [x] - `p1` - DB: list `in_progress` multipart sessions with `expires_at < now` - `inst-sweep-multipart-list`
2. [x] - `p1` - FOR EACH: CAS the session `in_progress -> aborted` **first**, via the same `Store::abort_multipart_upload` the user-driven abort path uses — that call also deletes the session's `multipart_upload_parts` rows in the same transaction as the state flip, so a concurrent `complete_multipart_upload` racing on the same session row can win instead (`in_progress -> completed`); only one side wins - `inst-sweep-multipart-cas`
3. [x] - `p1` - **IF** the sweep won the CAS: best-effort abort the backend upload handle, then delete the pending version row **status-guarded** (`status = pending` only) — a version a racing complete already flipped to `available` via `finalize_version` (ahead of its own session CAS) is left untouched; the DELETE simply matches zero rows - `inst-sweep-multipart-cleanup`
4. [x] - `p1` - **IF** the sweep lost the CAS (session already transitioned): skip version cleanup entirely and log — if the winner was `complete`, the version is now `Available` and bound; touching it would be data loss - `inst-sweep-multipart-skip`
5. [x] - `p1` - RETURN the count of sessions the sweep itself won and aborted - `inst-sweep-multipart-return`

### Sweep Retention-Policy Expiry

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-sweep-retention-expiry`

**Input**: `now`

**Output**: count of files deleted

**Steps**:
1. [x] - `p1` - DB: list all retention rules across all tenants and scopes; **IF** empty, skip the file scan entirely - `inst-sweep-retention-rules`
2. [x] - `p1` - Scan all files in keyset-paginated batches of 500 (by `file_id`, `after`-cursor), so the sweep never materializes every file across every tenant in memory regardless of deployment size - `inst-sweep-retention-scan`
3. [x] - `p1` - FOR EACH file in a batch: gather rules applicable by scope (`Tenant` → always; `User` → `rule.scope_target_id == file.owner_id`; `File` → `rule.scope_target_id == file.file_id`), restricted to the file's own tenant - `inst-sweep-retention-applicable`
4. [x] - `p1` - **IF** any applicable rule: fetch the file's custom metadata (needed for a metadata-criterion rule); a fetch failure skips the file (logged) rather than treating it as "no metadata, no match" - `inst-sweep-retention-metadata`
5. [x] - `p1` - Evaluate OR semantics across the file's applicable rules — the first matching criterion (age: `now - created_at > max_age_days`; inactivity: `now - last_modified_at > inactivity_days`, **not** reset by downloads, only by writes; metadata: an exact key/value match) triggers expiry - `inst-sweep-retention-match`
6. [x] - `p1` - **IF** expiring: write a `retention_delete` audit row and a `file.deleted` event on the same transactional-outbox path user-initiated deletes use, delete the file (all versions + the `files` row), debit total bytes and `file_count_delta = -1` via the usage reporter, then best-effort delete each version's backend blob - `inst-sweep-retention-delete`
7. [x] - `p1` - RETURN the total deleted across all pages - `inst-sweep-retention-return`

### Validate Retention Rule on Write

- [x] `p2` - **ID**: `cpt-cf-file-storage-algo-validate-retention-rule`

**Input**: `RetentionScope`, `scope_target_id: Option<Uuid>`, `RetentionRuleBody`

**Output**: `Ok(())`, or `DomainError::Validation`

Same spirit as the policy-engine's write-time validation: reject a body that would be
dangerous or permanently dead rather than silently accept it.

**Steps**:
1. [x] - `p2` - **IF** `age`, `inactivity`, and `metadata` are all absent: reject — the rule could never match any file - `inst-validate-retention-empty`
2. [x] - `p2` - **IF** `age.max_age_days < 1` or `inactivity.inactivity_days < 1` (i.e. `== 0`, both are `u32`): reject — `0` would match every file in the tenant on the very next sweep tick, and there is no dry-run or undo - `inst-validate-retention-zero`
3. [x] - `p2` - **IF** `scope ∈ {User, File}` and `scope_target_id` is absent: reject — a dead rule that can never resolve to a target file (this validation runs *before* authorization in `create_retention_rule`, so this check — not `authorize_retention_scope`'s `File` arm — is what catches a missing target for both `User`- and `File`-scope requests; `authorize_retention_scope` never sees a missing `File`-scope target, since a request that lacks one is already rejected by the time it would run) - `inst-validate-retention-target`
4. [x] - `p2` - RETURN `Ok(())` otherwise - `inst-validate-retention-return`

## 4. States (CDSL)

### Multipart Session (owned by multipart-coordinator, driven here on a timer)

- [x] `p1` - **ID**: `cpt-cf-file-storage-state-retention-cleanup-multipart-touch`

This feature does not define its own state machine for the multipart session — that is
[`cpt-cf-file-storage-state-multipart-session`](multipart-coordinator.md#multipart-session-state-machine), owned by
multipart-coordinator.md. This feature is the sole driver of that state machine's third transition
(`in_progress -> aborted` on TTL expiry) via [Sweep Expired Multipart
Sessions](#sweep-expired-multipart-sessions); it introduces no new state values.

**No cross-instance coordination in P2.** The sweep runs independently, unsynchronized, on every control-plane
instance on its own timer. This is deliberately safe rather than merely "not yet a bug": every mutation the sweep
performs is a CAS or a status-guarded delete, so a concurrent redundant sweep on the same row gets `Ok(false)`/zero
rows affected rather than corrupting state or double-reporting usage. Leader election / distributed locking to
eliminate the redundant work (not the small risk of incorrectness, since there is none) is deferred to P3.

## 5. Definitions of Done

### Retention Rule Domain Types and Administration Endpoints

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-retention-rule-endpoints`

`RetentionScope` (`Tenant`/`User`/`File`) and `RetentionRuleBody` (`age`/`inactivity`/`metadata`, OR
semantics — any one matching criterion triggers expiry) are defined in `src/domain/policy.rs`; `GET/POST /retention-rules` and
`DELETE /retention-rules/{rule_id}` (`src/api/rest/routes.rs:388-440`, `handlers::list_retention_rules`/
`create_retention_rule`/`delete_retention_rule`) are backed by `PolicyService::list_retention_rules`/
`create_retention_rule`/`delete_retention_rule` (`src/domain/policy_service.rs`). Scope-aware authorization (`Tenant`
= `ADMIN_POLICY` outright, no `WRITE` fallback; `User` = `ADMIN_POLICY`-first with `WRITE`-plus-target-match fallback; `File` = resolve-then-
per-file-`WRITE`) is covered by `tests/policy_authz_test.rs`
(`create_retention_rule_file_scope_target_not_writable_is_denied`,
`create_retention_rule_file_scope_target_writable_is_allowed`, `delete_retention_rule_foreign_owner_is_denied`,
`delete_missing_retention_rule_returns_retention_not_found`).

**Implements**:
- `cpt-cf-file-storage-flow-retention-list`
- `cpt-cf-file-storage-flow-retention-create`
- `cpt-cf-file-storage-flow-retention-delete`

**Touches**:
- API: `GET /api/file-storage/v1/retention-rules`, `POST /api/file-storage/v1/retention-rules`,
  `DELETE /api/file-storage/v1/retention-rules/{rule_id}`
- DB Table: `retention_rules`

### Cleanup Engine and Background Sweep Scheduling

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-cleanup-engine`

`CleanupEngine::run_sweep` (`src/domain/cleanup.rs`) implements all four steps in [Run Sweep
Cycle](#run-sweep-cycle). `gear.rs` spawns a `tokio::spawn` loop on `cfg.sweep_interval_secs`, gated by
`cfg.enable_background_sweep` (default enabled; test/dev harnesses that need deterministic behavior set it `false`
and call `run_sweep()` directly), and exports the `SweepResult` tallies as metrics counters
(`sweep_metrics.record_sweep_result`) at the point they are logged. Covered end-to-end by `tests/cleanup_test.rs`
(25 tests spanning all four steps, backend migration interaction, and idempotency/outbox housekeeping).

**Implements**:
- `cpt-cf-file-storage-algo-run-sweep`
- `cpt-cf-file-storage-algo-sweep-abandoned-pending`
- `cpt-cf-file-storage-algo-sweep-expired-multipart`
- `cpt-cf-file-storage-algo-sweep-retention-expiry`

**Touches**:
- Gears: `src/domain/cleanup.rs`, `src/gear.rs`
- DB Table: `file_versions`, `files`, `multipart_uploads`, `multipart_upload_parts`, `idempotency_keys`

> **Usage-reporting caveat, mirroring multipart-coordinator.md's own note.** `CleanupEngine::report_usage` is a real,
> wired call at every debit/credit point in the sweep (abandoned-pending reclaim, orphan-file delete,
> retention-expiry delete), but `gear.rs` constructs the engine `.with_usage_reporter(None)` — no Usage Collector
> client is wired in any deployment today, so every sweep-driven usage report is currently a fire-and-forget no-op.
> The reporting code path itself is exercised by unit tests injecting a mock reporter where present; this does not
> block or alter sweep correctness (usage reporting is `cpt-cf-file-storage-fr-usage-reporting`, a separate
> requirement from this FEATURE).

### Live-Multipart-Session Guard

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-cleanup-live-multipart-guard`

`VersionRepo::list_pending_older_than`
(`src/infra/storage/repo/version_repo.rs:373-403`) — the query backing [Sweep Abandoned Pending
Versions](#sweep-abandoned-pending-versions-orphan-reconciliation) — filters out any `pending` version row whose
`version_id` appears in a live multipart session: `SELECT version_id FROM multipart_uploads WHERE state =
'in_progress' AND expires_at > now`. **Invariant**: a pending version backing a still-`in_progress`, unexpired
multipart session is **never** selected for reclamation by step 1, regardless of how old its `created_at` is — a
long-running upload (large file, generous URL TTL) can legitimately keep its backing version `pending` for longer
than `orphan_grace_secs`, and without this guard the sweep would delete the version out from under the in-progress
upload. A session whose `expires_at` has **already passed** is deliberately **not** excluded by this guard — that
version becomes reclaimable, but only after [Sweep Expired Multipart
Sessions](#sweep-expired-multipart-sessions) (step 2, running in the same cycle) has transitioned the session out of
`in_progress`; step 1 and step 2 run in a fixed order within one `run_sweep` call, so a session that expires exactly
between them is reclaimed on the *next* cycle, not silently missed. The same live-session check is repeated,
independently, by `CleanupEngine::has_blocking_multipart_session` before deleting a permanently-orphaned zero-version
`files` row (§3, step 5's `inst-sweep-pending-orphan-file`), for the same reason at the file-deletion granularity: a
`files` row's `ON DELETE CASCADE` would otherwise take a still-`in_progress` `multipart_uploads` row down with it.

Directly exercised by `tests/cleanup_test.rs::sweep_skips_pending_version_of_active_multipart_session` (a backdated-
`created_at`, still-live session's version survives the sweep untouched) and its companion
`sweep_reclaims_version_after_session_expires` (once `expires_at` also passes, the session is aborted by step 2 and
its version is reclaimed by step 1 on the same `run_sweep()` call).

**Implements**:
- `cpt-cf-file-storage-algo-sweep-abandoned-pending`

**Touches**:
- Gears: `src/infra/storage/repo/version_repo.rs`, `src/domain/cleanup.rs`
- DB Table: `file_versions`, `multipart_uploads`

### Semantic Validation on Write

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-retention-semantic-validation`

`PolicyService::validate_retention_rule` rejects an all-criteria-absent body, a zero-day age/inactivity
criterion, and a `User`/`File`-scope rule with no target, at `POST /retention-rules` write time. Covered by
`tests/policy_authz_test.rs`'s `create_retention_rule_zero_max_age_is_rejected`,
`create_retention_rule_all_criteria_none_is_rejected`, `create_retention_rule_user_scope_without_target_is_rejected`.
`tests/cleanup_test.rs::sweep_does_not_run_zero_age_rule` additionally proves the guard is a real, load-bearing
gate rather than a redundant safety net: it attempts to create a zero-`max_age_days` rule through
`PolicyService::create_retention_rule` itself (rejected as `DomainError::Validation`, no row written), then confirms
a file that *would* have matched such a rule survives a subsequent sweep untouched. The companion
`retention_expired_file_is_deleted_by_sweep` test takes the opposite approach — inserting a zero-day rule directly
through the store, bypassing this write-time guard on purpose — specifically to exercise the sweep's own matcher
mechanics in isolation from the guard.

**Implements**:
- `cpt-cf-file-storage-algo-validate-retention-rule`

**Touches**:
- Gears: `src/domain/policy_service.rs`

## 6. Acceptance Criteria

- [x] Owners can define retention rules at tenant, user, or file scope, matching on age, inactivity, or a
  custom-metadata key/value, with OR semantics across multiple criteria on one rule
  (`cpt-cf-file-storage-fr-retention-policies`)
- [x] A file matched by any applicable retention rule is deleted (content + metadata + custom metadata, cascading)
  by the background sweep, with an audit record (`retention_delete`) and a `file.deleted` event on the same
  transactional-outbox path user-initiated deletes use
- [x] A retention rule that would match every file immediately (`max_age_days`/`inactivity_days == 0`) or could
  never match any file (all criteria absent) or could never resolve a target (`user`/`file` scope with no target) is
  rejected at write time, not silently accepted
- [x] A `pending` version row past `orphan_grace_secs` with no finalize/bind is deleted, along with its backend
  blob (best-effort), and an `orphan_reconcile` audit row is written (`cpt-cf-file-storage-fr-orphan-reconciliation`)
- [x] A file left with zero versions and `content_id IS NULL` after its last pending version is reclaimed is itself
  deleted (not left as a permanent, unreachable-forever `files` row), with a `file.deleted` event
- [x] A file that still has another (bound) version is never deleted by the zero-version-orphan check, even while
  one of its other versions is independently reclaimed as abandoned-pending
- [x] A `pending` version still backing a **live** (`in_progress`, unexpired) multipart session is **never** selected
  for orphan reclamation regardless of its age — this invariant is enforced both at the
  version-query level (`list_pending_older_than`'s `NOT IN` subquery against live sessions) and, independently, at
  the zero-version-orphan-file check (`has_blocking_multipart_session`)
- [x] Once that same session's `expires_at` has also passed, the session is aborted by the sweep's own step 2 and
  its previously-protected version becomes reclaimable by step 1 on a subsequent cycle
- [x] An expired multipart session is aborted via a CAS (`in_progress -> aborted`) that a concurrent
  `complete_multipart_upload` can win instead; the loser leaves the version completely untouched either way (no
  double-delete, no deleting a since-bound version)
- [x] Expired `idempotency_keys` rows are purged by the same sweep cycle; `audit_outbox`/`events_outbox` rows are
  never touched by any sweep step regardless of age, since `published_at` staying `NULL` cannot yet be distinguished
  from "not yet relayed" until the Tier-4 `EventBroker` relay exists
- [x] The sweep runs independently and without coordination on every control-plane instance; every mutation is a
  CAS or status-guarded delete, so redundant concurrent sweeps are safe (idempotent), merely duplicative of effort
- [ ] Backend blob-without-row reconciliation (a `list_paths` scan for backend objects with no matching version row)
  and flagging an `available` version with no matching backend object for operator attention — both named in the
  PRD's `fr-orphan-reconciliation` — are **not implemented**; deferred to P3 pending cross-instance leader election,
  which a per-instance `list_paths` scan cannot safely do without (see the scope note in §1.2)
- [ ] Non-current-version retention (pruning a superseded version once a newer one is bound, per a
  `keep_last_n`/`max_non_current_age_days`-style policy) is **not implemented** — no such field exists on
  `RetentionRuleBody` today; deferred to P3 pending a versioning-policy schema (see `lifecycle.rs`'s module doc
  comment)
- [ ] Sweep-driven usage reports (bytes/file-count debits on reclaim/expiry) are wired end-to-end in code but are a
  no-op in every real deployment today, since `gear.rs` wires no `UsageReporter` (`cpt-cf-file-storage-fr-usage-
  reporting` is a separate, not-yet-connected requirement — see the caveat under [Cleanup Engine and Background
  Sweep Scheduling](#cleanup-engine-and-background-sweep-scheduling))
