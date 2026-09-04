Created:  2026-07-08 by Constructor Tech
Updated:  2026-07-08 by Constructor Tech
# Feature: Policy Engine (Allowed Types + Size Limits)

- [ ] `p2` - **ID**: `cpt-cf-file-storage-featstatus-policy-engine-implemented`



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Get Own Policy](#get-own-policy)
  - [Set (Upsert) Policy](#set-upsert-policy)
  - [Get Effective Policy](#get-effective-policy)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Resolve Effective Policy (Most-Restrictive-Wins)](#resolve-effective-policy-most-restrictive-wins)
  - [Enforce Allowed-Types and Size Limits at Upload](#enforce-allowed-types-and-size-limits-at-upload)
  - [Validate Policy Body on Write](#validate-policy-body-on-write)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Policy Domain Types and Resolver](#policy-domain-types-and-resolver)
  - [GET/PUT /policy Endpoints](#getput-policy-endpoints)
  - [GET /policy/effective Endpoint](#get-policyeffective-endpoint)
  - [Enforcement Wired Into the Write Path](#enforcement-wired-into-the-write-path)
  - [Semantic Validation on Write](#semantic-validation-on-write)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

- [ ] `p2` - `cpt-cf-file-storage-feature-policy-engine`

### 1.1 Overview

Tenant- and user-scoped policy configuration for two aspects: **allowed MIME types** for upload
(`cpt-cf-file-storage-fr-allowed-types-policy`) and **file size limits**, global and per-mime
(`cpt-cf-file-storage-fr-size-limits-policy`). A policy may be set at the `tenant` scope (applies to every owner in
the tenant) or the `user` scope (applies to one owner). `PolicyResolver::resolve` computes the **effective policy**
for a request context as the **most-restrictive combination** of the tenant-level and user-level bodies, per aspect:
narrowest allowed-mime intersection, smallest global `max_bytes`, smallest per-mime override, smallest metadata
limit. The effective policy is enforced at every write path that creates or grows content (`create_file`,
`presign_version`, `finalize_upload`/`finalize_upload_by_token`, `update_metadata`, and multipart
`initiate`/`complete`) as well as exposed directly via `GET /policy/effective` for clients that want to pre-validate
before upload.

### 1.2 Purpose

Tenants need to restrict uploads to approved file types for security/compliance reasons (blocking executables, for
example) and need granular control over storage consumption via size ceilings, without one level (tenant admin vs.
individual user) being able to loosen a restriction the other level intended to be a hard ceiling. The
most-restrictive-wins resolution model (`PolicyResolver::resolve`) guarantees that combining a tenant policy and a
user policy can only ever narrow the effective policy, never widen it.

**Requirements**: `cpt-cf-file-storage-fr-allowed-types-policy`, `cpt-cf-file-storage-fr-size-limits-policy`,
`cpt-cf-file-storage-fr-metadata-limits` (the same `PolicyBody`/`EffectivePolicy` types and resolver also carry
metadata-limit resolution, sharing this feature's plumbing; metadata-limit *enforcement* call sites are documented
inline below for completeness but the FEATURE's owning requirement ids are the two named above)

**Principles**: `cpt-cf-file-storage-principle-control-no-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-file-storage-actor-platform-user` | Reads/writes tenant- or user-scope policy; uploads are checked against the effective policy computed from these bodies |
| `cpt-cf-file-storage-actor-cf-gears` | Peer gear / service subject to the same effective-policy enforcement on any write path it drives |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md) — §5.4 "Policies (Phase 2)": Allowed File Types Policy
  (`cpt-cf-file-storage-fr-allowed-types-policy`), File Size Limits Policy (`cpt-cf-file-storage-fr-size-limits-policy`)
- **Design**: [DESIGN.md](../DESIGN.md)
- **API contract**: [api.md](../api.md) — `GET/PUT /policy`, `GET /policy/effective`
- **Dependencies**: none (this feature has no dependency on multipart-coordinator.md or content-hash-modes.md; it is
  consumed BY the write paths those features own — `finalize_upload`'s defense-in-depth size check, multipart
  `initiate`'s allowed-mime/size gate — rather than depending on them)

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor and describe the end-to-end flow of a use case.

### Get Own Policy

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-policy-get-own`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Caller receives the raw (own-level, not resolved) policy body for the requested scope, if one has been set
- No policy configured at that scope — `204 No Content` (not an error)

**Error Scenarios**:
- `scope` is not `"tenant"` or `"user"` — `400`
- Caller lacks `READ` (and, for a foreign `scope_owner_id`, lacks `ADMIN_POLICY` too) — `403`

**Steps**:
1. [x] - `p1` - Client: GET /api/file-storage/v1/policy?scope={tenant|user}&scope_owner_id={uuid?} - `inst-policy-get-request`
2. [x] - `p1` - API: parse `scope`; `400` if neither `"tenant"` nor `"user"` - `inst-policy-get-parse-scope`
3. [x] - `p1` - Authorize: try `ADMIN_POLICY` on `("", None)` first (cross-owner/tenant-wide admin); on `Forbidden`, fall back to `READ` and require `scope_owner_id` (when present) to equal the caller's own subject id — a missing `scope_owner_id` (tenant-scope request) is treated as authorized on `READ` alone - `inst-policy-get-authz`
4. [x] - `p1` - DB: SELECT policy row for `(tenant_id, scope, scope_owner_id)` - `inst-policy-get-load`
5. [x] - `p1` - RETURN 200 with the stored policy, or 204 if none exists - `inst-policy-get-return`

### Set (Upsert) Policy

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-policy-set`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- The policy body for the scope is upserted (created or replaced in full); the stored row's `created_at`/`updated_at`
  are both set to the write's timestamp

**Error Scenarios**:
- `scope` is not `"tenant"` or `"user"` — `400`
- `scope = "user"` with no `scope_owner_id` — `400` (a user-scope row with no owner could never be read back)
- `allowed_mime_types` or `size_limits.per_mime` contains a `*/*` entry — `400` (rejected outright: `*/*` would
  silently match nothing against the wildcard matcher, which only special-cases the *subtype* half of a pattern —
  a caller that wants "no restriction" should omit the field entirely)
- `scope_owner_id` is absent (tenant scope) and the caller lacks `ADMIN_POLICY` — `403`, with no
  fallback to `WRITE`: a tenant-scope policy applies to every subject in the tenant (allowed mime
  types, size and metadata limits), so ordinary file-`WRITE` must not let an unprivileged tenant
  member unilaterally tighten or loosen it for everyone
- `scope_owner_id` is present and the caller lacks `WRITE` (or, for a foreign `scope_owner_id`, lacks
  `ADMIN_POLICY` too) — `403`

**Steps**:
1. [x] - `p1` - Client: PUT /api/file-storage/v1/policy {scope, scope_owner_id?, body} - `inst-policy-set-request`
2. [x] - `p1` - API: parse `scope`; `400` if invalid - `inst-policy-set-parse-scope`
3. [x] - `p1` - Validate: reject `scope = User` with no `scope_owner_id`, and reject any `*/*` mime pattern in `allowed_mime_types`/`size_limits.per_mime` - `inst-policy-set-validate` (runs before authorization: a `User`-scope request missing its owner is a malformed body, `400`, not an administrative act — validating first means it is never misreported as `403` just because a missing owner also happens to be how tenant scope is spelled)
4. [x] - `p1` - Authorize: if `scope_owner_id` is absent (tenant scope), require `ADMIN_POLICY` outright with no fallback; otherwise the same `ADMIN_POLICY`-first, `WRITE`-plus-owner-match fallback as [Get Own Policy](#get-own-policy), with `WRITE` instead of `READ` as the fallback action - `inst-policy-set-authz`
5. [x] - `p1` - DB: upsert the `(tenant_id, scope, scope_owner_id)` row transactionally (partial-unique-index backstop; two sequential upserts for the same scope leave exactly one row, never a duplicate) - `inst-policy-set-upsert`
6. [x] - `p1` - RETURN 200 with the stored policy (`created_at`/`updated_at` both the write's timestamp) - `inst-policy-set-return`

### Get Effective Policy

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-policy-get-effective`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Caller receives the effective (most-restrictive-across-levels) policy: `allowed_mime_types` (intersection, `null`
  = all permitted), `max_bytes` (smallest non-`null`), `per_mime_max_bytes` (union of patterns, tightened by any
  covering wildcard), `metadata_limits` (smallest non-`null` per field). Passing `user_owner_id` includes that
  user's policy in the resolution; omitting it resolves tenant-level only

**Error Scenarios**:
- Caller lacks `READ` — `403`
- `user_owner_id` is present and differs from the caller's own subject id, and the caller lacks
  `ADMIN_POLICY` — `403` (plain `READ` only clears the tenant policy and the caller's own user
  policy; without this check any tenant member could pass a victim's id and have that user's policy
  merged into the response — a policy-disclosure side channel)

**Steps**:
1. [x] - `p1` - Client: GET /api/file-storage/v1/policy/effective?user_owner_id={uuid?} - `inst-policy-eff-request`
2. [x] - `p1` - Authorize `READ` on `("", None)`; if `user_owner_id` is present and differs from the caller's own subject id, additionally require `ADMIN_POLICY` - `inst-policy-eff-authz`
3. [x] - `p1` - DB: SELECT the tenant-scope policy row (always) and, if `user_owner_id` is present, the user-scope row for that owner - `inst-policy-eff-load`
4. [x] - `p1` - Algorithm: `cpt-cf-file-storage-algo-resolve-effective-policy` combines the two (either or both may be absent) - `inst-policy-eff-resolve`
5. [x] - `p1` - RETURN 200 with the resolved `EffectivePolicy` - `inst-policy-eff-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions that do not interact with actors directly; called by the flows above and by every
content-write path this feature protects.

### Resolve Effective Policy (Most-Restrictive-Wins)

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-resolve-effective-policy`

**Input**: `tenant_policy: Option<&PolicyBody>`, `user_policy: Option<&PolicyBody>` (either or both may be absent —
absence contributes no restriction from that level)

**Output**: `EffectivePolicy { allowed_mime_types, max_bytes, per_mime_max_bytes, metadata_limits }`

**Steps**:
1. [x] - `p1` - Allowed mime types: a level is "restricted" only when its `allowed_mime_types` is non-empty (empty means unrestricted at that level, not "nothing allowed"). Both unrestricted → `None` (all permitted). One restricted → that level's set. Both restricted → intersection, resolved to the **narrower** pattern per overlapping pair (`image/*` ∩ `image/png` = `image/png`, not `image/*`) - `inst-resolve-mime`
2. [x] - `p1` - Global size limit: `min(tenant.max_bytes, user.max_bytes)`, `None` treated as unlimited (not zero) - `inst-resolve-size`
3. [x] - `p1` - Per-mime overrides: union of patterns from both levels (identical pattern takes the smaller value), then a second pass tightens every entry by any *broader* pattern that also covers it (a `image/* = 10MB` wildcard cap always tightens a more-specific `image/png = 50MB` entry down to `10MB`, so a consumer that only ever looks at the most-specific matching entry can never see a looser effective value than a covering wildcard intended) - `inst-resolve-per-mime`
4. [x] - `p1` - Metadata limits: smallest non-`None` value from each of `max_pairs`/`max_key_len`/`max_value_len`/`max_total_bytes`, independently per field - `inst-resolve-metadata`
5. [x] - `p1` - RETURN the combined `EffectivePolicy` - `inst-resolve-return`

### Enforce Allowed-Types and Size Limits at Upload

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-enforce-policy-at-upload`

**Input**: `EffectivePolicy`, the mime type in question, the byte size in question (declared or actual, depending
on call site), the backend's hardware `max_size_bytes` ceiling (if any)

**Output**: `Ok(())`, or `DomainError::PolicyMimeNotAllowed`/`DomainError::PolicySizeExceeded`

This single pair of helpers (`PolicyResolver::check_allowed_mime`, `PolicyResolver::compute_effective_max_bytes`) is
called at **every** content-write entry point rather than each path re-implementing the check:

- `create_file` (`create.rs:189-210`) — allowed-mime and size against `new.mime_type`, before the upload URL is
  even minted
- `presign_version` (`create.rs:341-354`) — same, for a subsequent version on an existing file
- `finalize_upload` (`write.rs:131-152`) and `finalize_upload_by_token` (`write.rs:615-647`) — a **defense-in-depth**
  re-check of the size ceiling at finalize time even though the sidecar already enforced the upload constraint
  baked into the signed URL; `finalize_upload`/`finalize_upload_by_token` additionally re-run
  `enforce_size_ceiling_for_validated_mime` (`write.rs:186-192`, `:681-687`) after MIME-sniffing the read-back
  bytes, so a client that lies about `Content-Type` in the declared MIME cannot bypass a per-mime size override
  keyed to the real, sniffed type
- Multipart `initiate_multipart_upload` (`multipart_service.rs:435-462`) — allowed-mime and size against the
  **declared** total size, checked up front at initiate rather than deferred to complete
- Multipart `complete_multipart_upload` (`multipart_service.rs:778-797`) — a residual size check against the
  **assembled** total, catching a mismatch the per-part sidecar enforcement and the size-verify step ahead of it
  did not
- `create_file`'s idempotency-replay path (`create.rs:153-252`, when a stored `idempotency_key` record matches the
  retried request) re-validates allowed-mime and metadata limits against the **current** effective policy rather
  than the policy in effect at the original call, recomputes the effective size ceiling from that current policy
  and re-mints the upload URL under it, and re-runs the quota preflight — so a policy tightened (or a quota
  exhausted) after the original `create_file` call is enforced on every replay for as long as the idempotency
  window stays open

**Steps**:
1. [x] - `p1` - `check_allowed_mime`: `None` `allowed_mime_types` on the effective policy permits everything; `Some([])` permits nothing; `Some(list)` requires an exact match or a `type/*` wildcard match - `inst-enforce-mime`
2. [x] - `p1` - `compute_effective_max_bytes`: take `min` of the backend's hardware ceiling, the policy's global `max_bytes`, and the smallest matching per-mime override — the effective ceiling is always the smallest of the three, so a per-mime override can only tighten the global limit, never exceed it; `None` in all three means unbounded - `inst-enforce-size-compute`
3. [x] - `p1` - Compare the candidate size against the computed ceiling; `DomainError::policy_size_exceeded` if over - `inst-enforce-size-compare`
4. [x] - `p1` - RETURN `Ok(())` if both checks pass - `inst-enforce-return`

> **Status code note.** `DomainError::PolicyMimeNotAllowed` and `DomainError::PolicySizeExceeded` both map to HTTP
> **`400`** at the REST boundary (`src/api/rest/error.rs`'s `FileResourceError::invalid_argument()`/
> `out_of_range()`, both of which `error_mapping_test.rs`'s exhaustive `DomainError → status` guardrail pins to
> `400`) — not `415`/`413` as the in-code doc-comments on `DomainError::PolicyMimeNotAllowed`/`PolicySizeExceeded`
> (`domain/error.rs`) suggest. There is no canonical-error variant on this platform that resolves to `415` or
> `413`; every policy rejection surfaces as a `400` field-violation Problem. `DomainError::PolicyMetadataExceeded`
> is likewise `400`, not the `422` its own doc-comment claims.

### Validate Policy Body on Write

- [x] `p2` - **ID**: `cpt-cf-file-storage-algo-validate-policy-body`

**Input**: `PolicyScope`, `scope_owner_id: Option<Uuid>`, `PolicyBody` (the incoming `PUT /policy` request)

**Output**: `Ok(())`, or `DomainError::Validation`

Reject a policy body that would be silently dead or dangerous rather than accept and never
detect it.

**Steps**:
1. [x] - `p2` - **IF** `scope == User` AND `scope_owner_id` is `None`: reject — the effective-policy reader always queries the user-scope row with `Some(owner_id)`, so a `None`-owner user-scope row could never be read back - `inst-validate-user-owner`
2. [x] - `p2` - **IF** `allowed_mime_types` contains `"*/*"`: reject — `*/*` splits into a base type of `"*"`, which never equals a real mime type's base, so it silently matches **nothing** (an accidental deny-all) rather than the "allow everything" the caller almost certainly intended; the correct way to express "no restriction" is to omit the field entirely - `inst-validate-star-slash-star-allowed`
3. [x] - `p2` - **IF** `size_limits.per_mime` contains an entry with `mime == "*/*"`: reject, same reasoning — use `size_limits.max_bytes` for a global limit instead - `inst-validate-star-slash-star-per-mime`
4. [x] - `p2` - RETURN `Ok(())` otherwise - `inst-validate-return`

## 4. States (CDSL)

**Not applicable.** A policy row is a plain key-value configuration record with no lifecycle of its own — it is
created, replaced in full on every `PUT` (upsert, never a partial patch), and read; there are no states, guards, or
transitions to model.

## 5. Definitions of Done

### Policy Domain Types and Resolver

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-policy-types-resolver`

`src/domain/policy.rs` defines `PolicyScope` (`Tenant`/`User`), `PolicyBody` (`allowed_mime_types`, `size_limits`, `metadata_limits`,
`enabled_event_types`), `EffectivePolicy`, and `PolicyResolver::resolve`/`check_allowed_mime`/
`compute_effective_max_bytes`/`check_metadata_limits`, with unit coverage in
`src/domain/policy_tests.rs` (resolver merge behavior) and `src/domain/service/service_tests.rs` (the enforcement
helpers, DB-free).

**Not enforced**: `enabled_event_types` is stored and round-tripped through `GET`/`PUT /policy` like every other
`PolicyBody` field, but `PolicyResolver` has no method that consults it, and no file-event enqueue path
(`FileService::make_file_event` call sites, or the cleanup engine's own `FileEvent` construction) checks it before
enqueuing. Every event type is enqueued unconditionally regardless of what a policy's `enabled_event_types` says —
the field is inert configuration, not enforced gating. See
[docs/migration.sql](../migration.sql)'s `events_outbox` table comment and
[docs/features/audit-trail.md](audit-trail.md) for the sibling outbox's related behavior.

**Implements**:
- `cpt-cf-file-storage-algo-resolve-effective-policy`

**Touches**:
- Gears: `src/domain/policy.rs`

### GET/PUT /policy Endpoints

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-policy-get-put-endpoints`

`GET /api/file-storage/v1/policy` and `PUT /api/file-storage/v1/policy`
(`src/api/rest/routes.rs:324-363`, `handlers::get_policy`/`set_policy`) are backed by `PolicyService::get_own_policy`/
`set_policy` (`src/domain/policy_service.rs`). Authorization: `get_own_policy` and `set_policy`'s `Some(scope_owner_id)`
branch use `ADMIN_POLICY`-first with a `READ`/`WRITE`-plus-owner-match fallback (`authorize_scope_owner`/
`authorize_admin_or_owner`); `set_policy`'s tenant-scope branch (`scope_owner_id = None`) requires `ADMIN_POLICY`
outright with no fallback, since a tenant-scope write applies to every subject in the tenant. Covered by `tests/policy_authz_test.rs`
(`set_policy_foreign_owner_without_admin_scope_is_denied`, `set_policy_self_owner_is_allowed`,
`set_policy_tenant_admin_scope_allows_foreign_owner`, `set_policy_user_scope_without_owner_is_rejected`,
`set_policy_star_slash_star_mime_is_rejected_or_defined`). Upsert race-safety (two sequential upserts for the same
scope leave exactly one row) covered by `tests/policy_test.rs`.

**Implements**:
- `cpt-cf-file-storage-flow-policy-get-own`
- `cpt-cf-file-storage-flow-policy-set`
- `cpt-cf-file-storage-algo-validate-policy-body`

**Touches**:
- API: `GET /api/file-storage/v1/policy`, `PUT /api/file-storage/v1/policy`
- DB Table: `policies`

### GET /policy/effective Endpoint

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-policy-effective-endpoint`

`GET /api/file-storage/v1/policy/effective` (`routes.rs:365-386`, `handlers::get_effective_policy`) is
backed by `PolicyService::get_effective_policy`, gated on plain `READ` — plus `ADMIN_POLICY` when the
query's `user_owner_id` differs from the caller's own subject id, to prevent using it as a
policy-disclosure side channel against another user.

**Implements**:
- `cpt-cf-file-storage-flow-policy-get-effective`
- `cpt-cf-file-storage-algo-resolve-effective-policy`

**Touches**:
- API: `GET /api/file-storage/v1/policy/effective`
- DB Table: `policies`

### Enforcement Wired Into the Write Path

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-policy-enforcement-wiring`

Every content-write entry point (`create_file`, `presign_version`, `finalize_upload`,
`finalize_upload_by_token`, `update_metadata`, multipart `initiate_multipart_upload`,
`complete_multipart_upload`) resolves the effective policy and calls the shared `PolicyResolver` enforcement
helpers rather than re-implementing the check. Covered by `tests/enforce_test.rs` (mime/size/metadata rejection at
the service layer) and `tests/multipart_test.rs` (`PolicySizeExceeded` at multipart initiate).

**Implements**:
- `cpt-cf-file-storage-algo-enforce-policy-at-upload`

**Touches**:
- Gears: `src/domain/service/create.rs`, `src/domain/service/write.rs`, `src/domain/multipart_service.rs`

### Semantic Validation on Write

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-policy-semantic-validation`

`PolicyService::validate_policy_body` rejects a user-scope policy with no `scope_owner_id` and any
`*/*` mime pattern, at `PUT /policy` write time. Covered by
`tests/policy_authz_test.rs`'s `set_policy_user_scope_without_owner_is_rejected` and
`set_policy_star_slash_star_mime_is_rejected_or_defined`.

**Implements**:
- `cpt-cf-file-storage-algo-validate-policy-body`

**Touches**:
- Gears: `src/domain/policy_service.rs`

## 6. Acceptance Criteria

- [x] Owners can define an `allowed_mime_types` policy at tenant or user scope; uploads of a disallowed type are
  rejected (`cpt-cf-file-storage-fr-allowed-types-policy`)
- [x] Owners can define a global `size_limits.max_bytes` and per-mime overrides at tenant or user scope; uploads
  exceeding the effective limit are rejected (`cpt-cf-file-storage-fr-size-limits-policy`)
- [x] When both a tenant-level and a user-level policy apply, the effective policy is the most-restrictive
  combination per aspect — a user-level policy can only narrow, never widen, what the tenant level set (and
  vice versa)
- [x] `GET /policy/effective` lets a caller pre-compute what an upload would be checked against, without attempting
  the upload
- [x] `PUT /policy` upsert is race-safe: two sequential upserts for the same `(tenant_id, scope, scope_owner_id)`
  leave exactly one row carrying the latest body, never a duplicate
- [x] A user-scope policy write without `scope_owner_id`, or any `*/*` mime pattern in `allowed_mime_types`/
  `size_limits.per_mime`, is rejected at write time rather than silently accepted as a dead or accidental deny-all
  entry
- [x] Policy read authorization (`get_own_policy`) tries `ADMIN_POLICY` first (cross-owner/tenant-wide
  administration) and falls back to `READ` plus an owner-match check for self-service tenant members;
  a tenant-scope read (no owner to compare) succeeds on `READ` alone. `set_policy`'s user-scope write
  follows the same `ADMIN_POLICY`-first, `WRITE`-plus-owner-match pattern, but its tenant-scope write
  requires `ADMIN_POLICY` outright with no fallback — there is no owner to fall back to self-service
  for, and a tenant-scope write changes policy for every subject in the tenant
- [ ] `PolicyMimeNotAllowed`/`PolicySizeExceeded`/`PolicyMetadataExceeded` are documented in their own
  `domain/error.rs` doc-comments as `415`/`413`/`422` respectively, but the platform's actual canonical-error
  mapping (pinned by `tests/error_mapping_test.rs`) resolves **all three to `400`** — those doc-comments are stale;
  `400` is the tested behavior for every policy rejection at every call site listed in [Enforce Allowed-Types and
  Size Limits at Upload](#enforce-allowed-types-and-size-limits-at-upload)
- [ ] `cpt-cf-file-storage-fr-storage-quota` (a related but distinct requirement, not owned by this FEATURE) is
  **not enforced in any real deployment** — `gear.rs` always wires `quota_client: None` — so a size-limits-policy
  rejection and a quota rejection are not equally reachable in production today; this FEATURE's own allowed-types
  and size-limits checks (unlike quota) run unconditionally and are exercised in every deployment
