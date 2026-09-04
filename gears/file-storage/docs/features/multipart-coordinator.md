Created:  2026-07-02 by Constructor Tech
Updated:  2026-07-02 by Constructor Tech
# Feature: Multipart Upload Coordinator

- [ ] `p1` - **ID**: `cpt-cf-file-storage-featstatus-multipart-coordinator-implemented`



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Initiate Multipart Upload](#initiate-multipart-upload)
  - [Upload a Part](#upload-a-part)
  - [Complete Multipart Upload](#complete-multipart-upload)
  - [Abort Multipart Upload](#abort-multipart-upload)
  - [Introspect and Resume Multipart Upload](#introspect-and-resume-multipart-upload)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Compute Parts Plan](#compute-parts-plan)
  - [Enforce Per-Part Size Claim at Sidecar](#enforce-per-part-size-claim-at-sidecar)
  - [Combine Part Hashes at Complete](#combine-part-hashes-at-complete)
- [4. States (CDSL)](#4-states-cdsl)
  - [Multipart Session State Machine](#multipart-session-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Initiate Endpoint with Server-Authoritative Plan](#initiate-endpoint-with-server-authoritative-plan)
  - [Sidecar Per-Part Enforcement](#sidecar-per-part-enforcement)
  - [Complete Endpoint with Hash Combination](#complete-endpoint-with-hash-combination)
  - [Abort Endpoint](#abort-endpoint)
  - [Introspect and Resume Endpoint](#introspect-and-resume-endpoint)
  - [Schema: multipart_uploads Plan Columns](#schema-multipart_uploads-plan-columns)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

## 1. Feature Context

- [ ] `p2` - `cpt-cf-file-storage-feature-multipart-coordinator`

### 1.1 Overview

Server-authoritative multipart upload coordinator for file-storage: the client declares total size and a preferred part size; the control plane computes the exact parts plan (part_number, offset, size) and returns one signed sidecar URL per part. The client uploads each part directly to the sidecar, which enforces the declared size before writing any bytes. The control plane then combines per-part hashes into the root hash at complete and binds the new file version atomically.

**Traces to**: `cpt-cf-file-storage-fr-multipart-upload`, `cpt-cf-file-storage-fr-size-limits-policy`, `cpt-cf-file-storage-fr-storage-quota`

### 1.2 Purpose

Provide a safe, resumable, server-controlled multipart upload path. Each part's exact byte length is a claim inside its signed URL, and the sidecar enforces that claim with HTTP 413 before writing any bytes, so a client cannot get oversized bytes to the backend regardless of the `declared_size` it declares up front.

All part bytes flow exclusively through sidecar signed URLs (ADR-0004); there is no control-plane byte route (ADR-0003) -- part bytes are never PUT to the control plane. See [Combine Part Hashes at Complete](#combine-part-hashes-at-complete) for how the content hash is computed.

**Requirements**: `cpt-cf-file-storage-fr-multipart-upload`, `cpt-cf-file-storage-fr-size-limits-policy`, `cpt-cf-file-storage-fr-storage-quota`

**Principles**: `cpt-cf-file-storage-principle-control-no-content`, `cpt-cf-file-storage-principle-signed-urls`

> **Caveat**: `LocalFsBackend.multipart_native == false` (it advertises `range_native: true` only), so `POST /files/{id}/multipart` returns `422 multipart_not_supported` against the default `local-fs` backend topology. Multipart uploads work only when a `multipart_native` backend is configured as the default: the non-durable `InMemoryBackend` (dev/test only), or the S3 backend (opt-in via `s3_backends` config, release-gated by ADR-0005). `LocalFsBackend` has no multipart support of its own -- it inherits the trait's default `Err(multipart_not_supported)` for `initiate_multipart`/`upload_part`/`complete_multipart`/`abort_multipart`.

> **Caveat**: `initiate`'s `check_quota_bytes` call
> (`multipart_service.rs`) is real and fail-closed when a `QuotaClient` is wired, but `gear.rs` always constructs
> `MultipartService` with `quota_client: None`. No `QuotaClient` is wired in any deployment, so
> the quota check is a permissive/fail-**open** no-op — the `declared_size exceeds available storage quota` error
> scenario below and the quota DoD line further down are exercised only by unit tests that inject a mock
> `QuotaClient` (`tests/enforce_test.rs`), not by any real deployment. There is no Quota Enforcement SDK crate wired
> up yet; `gears/system/quota-enforcement/` is docs-only. See `../operations.md#storage-quota-not-enforced`.



### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-file-storage-actor-platform-user` | Initiates a multipart upload by declaring intent; receives the parts plan; PUTs each part body to the sidecar URL; calls complete or abort |
| `cpt-cf-file-storage-actor-cf-gears` | Peer gear / service that drives multipart upload on behalf of a user; also subject to the same plan, quota, and enforcement rules |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) -- Section 4.6 (Multipart upload shape)
- **API contract**: [api.md](../api.md) -- P2 Multipart upload endpoints
- **ADR**: [ADR-0002](../ADR/0002-cpt-cf-file-storage-adr-content-hash-selection.md) -- Content hash selection; its `hash_policy` vision is superseded by ADR-0006
- **ADR**: [ADR-0006](../ADR/0006-cpt-cf-file-storage-adr-content-hash-modes.md) -- Content-hash modes (whole-object + multipart offset-manifest composite, SHA-256-only)
- **ADR**: [ADR-0003](../ADR/0003-cpt-cf-file-storage-adr-sidecar-data-plane.md) -- Sidecar data plane (no bytes through control plane)
- **ADR**: [ADR-0004](../ADR/0004-cpt-cf-file-storage-adr-signed-url-transport.md) -- Signed-URL transport, implemented as a codec-equivalent bespoke Ed25519 token, not literal PASETO v4.public -- no `kid` -- see that ADR's "Implementation note"
- **Dependencies**: Signed-URL transport (ADR-0004), sidecar data plane (ADR-0003)

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case.

### Initiate Multipart Upload

- [ ] `p1` - **ID**: `cpt-cf-file-storage-flow-multipart-initiate`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Client receives the exact parts plan (part_number, offset, size) and one signed sidecar URL per part; a pending version is pre-registered; multipart session is in_progress

**Error Scenarios**:
- `declared_mime` rejected by effective allowed-types policy -- 415 Unsupported Media Type
- `declared_size` exceeds effective size limit -- 413 Content Too Large
- `declared_size` exceeds available storage quota -- 429 Too Many Requests
- File not found or client lacks write permission -- 404 / 403

**Steps**:
1. [ ] - `p1` - Client: POST /api/file-storage/v1/files/{id}/multipart with body {declared_mime, declared_size, preferred_part_size?, concurrency?} - `inst-init-request`
2. [ ] - `p1` - API: validate declared_mime against the effective allowed-types policy; RETURN 415 if rejected - `inst-init-mime-check`
3. [ ] - `p1` - API: validate declared_size <= effective per-file size limit; RETURN 413 if exceeded - `inst-init-size-check`
4. [ ] - `p1` - API: validate declared_size against storage quota; RETURN 429 if exceeded - `inst-init-quota-check`
5. [ ] - `p1` - Algorithm: compute parts plan using `cpt-cf-file-storage-algo-compute-parts-plan` - `inst-init-plan`
6. [ ] - `p1` - DB: INSERT into multipart_uploads (upload_id, file_id, version_id, declared_size, part_size, status=in_progress, expires_at) - `inst-init-db-session`
7. [ ] - `p1` - DB: INSERT pending version row into file_versions (version_id, file_id, status=pending) - `inst-init-db-version`
8. [ ] - `p1` - FOR EACH part in the plan: mint a signed URL (Ed25519, codec-equivalent to PASETO v4.public -- ADR-0004's Implementation note) with claims {upload_id, file_id, version_id, part_number, offset, size, op="multipart_part", exp} - `inst-init-sign-urls`
9. [ ] - `p1` - RETURN 200 {upload_id, version_id, part_hash_algorithm, part_size, parts: [{part_number, offset, size, upload_url}], expires_at} - `inst-init-return`

### Upload a Part

- [x] `p1` - **ID**: `cpt-cf-file-storage-flow-multipart-upload-part`

**Actor**: `cpt-cf-file-storage-actor-platform-user` (part PUT goes directly to sidecar, not through control plane)

**Success Scenarios**:
- Part body is accepted, written, and its hash is persisted (via the sidecar's report-part callback to the control
  plane -- the sidecar itself has no DB connection, ADR-0003); re-PUT of same (upload_id, part_number) is idempotent
  (overwrite)

**Error Scenarios**:
- Request body length does not match the size claim in the signed token -- 413 before any bytes written
- Signed token is invalid, expired, or tampered -- 403
- Sidecar backend write failure -- 500

**Steps**:
1. [x] - `p1` - Client: PUT <signed_part_url> with raw body of exactly `size` bytes - `inst-part-request`
2. [x] - `p1` - Sidecar: verify the signed token (asymmetric Ed25519; sidecar cannot mint tokens -- ADR-0004; **not
   literal PASETO -- see that ADR's Implementation note**) - `inst-part-verify-token`
3. [x] - `p1` - **IF** token invalid or expired **RETURN** 403 Forbidden - `inst-part-token-reject`
4. [x] - `p1` - Algorithm: enforce per-part size claim using `cpt-cf-file-storage-algo-enforce-part-size` -- RETURN 413/PAYLOAD_TOO_LARGE before/after writing if mismatch - `inst-part-size-enforce`
5. [x] - `p1` - **IF** backend is multipart_native (`InMemoryBackend` or `S3Backend`): call backend PutPart(upload_handle, part_number, body) - `inst-part-write-native`
6. [x] - `p1` - **ELSE** write the part to a **separate per-part backend object** `{backend_path}.part.{part_number}` (`bin/sidecar.rs`) -- **not** an offset-write into the shared `/{file_id}/{version_id}` object. Only a non-`multipart_native` backend takes this branch; `local-fs` (the only registered backend with `multipart_native == false`) is rejected at initiate time (see the caveat above), so no currently-registered backend reaches this branch in production - `inst-part-write-offset`
7. [x] - `p1` - Sidecar: compute the SHA-256 hash of the written part bytes - `inst-part-hash`
8. [x] - `p1` - Sidecar: POST the report-part callback to the control plane, `.../versions/{version_id}/multipart/{upload_id}/parts/{part_number}/report {backend_etag, hash_hex, size}`, authorized solely by the same signed `fs-token` (no separate app-token, no on-behalf-of delegation) - `inst-part-report`
9. [x] - `p1` - Control plane: UPSERT multipart_upload_parts (upload_id, part_number, backend_etag, part_hash, size) -- idempotent overwrite on re-PUT/re-report - `inst-part-db-upsert`
10. [x] - `p1` - Sidecar: RETURN 200 {part_number, etag, hash_algorithm, hash} to the client once the report-part callback succeeds (no `size` field in this response body) - `inst-part-return`

### Complete Multipart Upload

- [ ] `p1` - **ID**: `cpt-cf-file-storage-flow-multipart-complete`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

`complete` returns **`200`** with a JSON body
(`version_id`, `size`, `hash_algorithm`, `content_hash`, `hash_mode`, `part_count`, `manifest`, `bind_state`,
`etag?`, `current_etag?`). It accepts an
**optional** `If-Match` header: a concrete value is checked against the file's current content ETag and a mismatch
is rejected (`400` -- `FailedPrecondition` collapses to `400` on this platform, there is no `412`-mapped
canonical-error variant); `*` or an absent header is unconditional. Before assembling, `complete` diffs the plan's
expected part numbers against the parts actually reported and returns `409` with the missing part numbers if any are
absent, in front of the generic `SUM(part.size) != declared_size` check. `complete` **finalizes** the version
(`pending -> available`, real assembled size/hash), and whether it also binds it depends on the session's
`auto_bind` flag, which is fixed at initiate time and cannot change mid-session:
- **Manual session** (`auto_bind = false` -- every session opened via the standalone `POST
  /files/{id}/multipart`, or via `POST /files`'s multipart block with `bind: "manual"`): `content_id` is left
  untouched, exactly like single-shot upload's manual finalize/bind split (ADR-0003, DESIGN.md §3.6).
  `bind_state` comes back `"manual"`. The client separately issues a `POST /files/{id}/bind {version_id}` under
  `If-Match` afterwards to make the assembled content live -- `complete`'s own `If-Match` only guards against the
  file's content pointer moving out from under a concurrent complete before that later bind, not the bind itself.
- **Auto-bind session** (`auto_bind = true` -- the default for `POST /files`'s multipart block, `bind: "auto"`):
  the bind runs **inside the same database transaction** that finalizes the version
  (`Store::finalize_version`, `src/infra/storage/store/versions.rs`) under a `content_id IS NULL` CAS -- no
  separate client `bind` request. A won CAS returns `bind_state: "bound"` with `etag` set to the new content
  `ETag`. A lost CAS (e.g. two create-tokens racing on the same brand-new file) still finalizes the version but
  leaves `content_id` untouched, returning `bind_state: "conflict"` with `current_etag` set to the ETag a manual
  `POST /files/{id}/bind` (as a fallback) would need for `If-Match`.

`complete` computes the version's content hash per ADR-0006's **`multipart-composite-sha256`** mode
(`cpt-cf-file-storage-adr-content-hash-modes`): the backend builds an offset-manifest from the per-part digests
already persisted in `multipart_upload_parts` (`part_hash`, `offset`) and the root hash is `sha256(manifest)` --
**no re-read/re-assembly of the object** is performed to compute it. The manifest text is persisted in
`version_hash_manifest` and returned verbatim in the `complete` response body (`manifest` field) so a client can
independently re-verify the composite hash (see [content-hash-modes.md](./content-hash-modes.md) §"Client-Side
Manifest Re-Verification").

**Success Scenarios**:
- All reported parts are assembled and verified by the backend; the version is **finalized** (`pending -> available`)
  with the real assembled size + ADR-0006 composite hash; session marked completed; the response body carries
  `version_id`/`size`/`hash_algorithm`/`content_hash`/`hash_mode`/`part_count`/`manifest`/`bind_state`(`/etag`/
  `current_etag`). For a **manual** session, binding the version as the file's current content is a **separate**,
  later `POST /files/{id}/bind` call -- `complete` does not bind (`bind_state: "manual"`). For an **auto-bind**
  session, `complete` binds the version itself in the same transaction as finalize (`bind_state: "bound"`), or
  reports a lost CAS without binding (`bind_state: "conflict"`, resolved by a manual `bind` call)

**Error Scenarios**:
- One or more planned parts have not been reported yet -- `409 Conflict` with the missing part numbers in the error
  detail (checked **before** the size-mismatch guard below)
- Assembled `SUM(part.size)` != `declared_size` (residual guard, e.g. an over-reported part) -- `409 Conflict`
- Policy size limit exceeded by the assembled total -- policy-size-exceeded error
- Session not `in_progress` (already completed/aborted, or foreign to this `file_id`) -- `404`-shaped "not found" / conflict
- `If-Match` supplied and it does not match the file's current content ETag -- `400` (`FailedPrecondition`)

**Steps**:
1. [x] - `p1` - Client: POST /api/file-storage/v1/files/{id}/multipart/{upload_id}/complete (no request body; optional `If-Match` header). Control plane: authorize `write`; if `If-Match` is present and not `*`, compare it against the file's current content ETag and reject on mismatch - `inst-complete-request`
2. [x] - `p1` - Control plane: load the session by `upload_id`; verify it belongs to `file_id` and is `in_progress` and not expired - `inst-complete-load-session`
3. [x] - `p1` - DB: SELECT all reported rows from multipart_upload_parts WHERE upload_id = ? - `inst-complete-load-parts`
4. [x] - `p1` - Diff the plan's expected part numbers (`ceil(declared_size / part_size)`) against the reported part numbers; RETURN 409 with the missing part numbers if any are absent - `inst-complete-missing-parts`
5. [x] - `p1` - Verify `SUM(part.size) == declared_size` (residual guard); RETURN 409 (generic) if mismatch - `inst-complete-size-verify`
6. [x] - `p1` - Policy size check against the assembled total - `inst-complete-policy-check`
7. [x] - `p1` - Backend: `CompleteMultipartUpload`, building the ADR-0006 offset-manifest and its root hash from the already-persisted per-part digests+offsets -- **no re-read** of the assembled object - `inst-complete-assemble`
8. [x] - `p1` - Post-assembly, pre-finalize: sniff the assembled object's leading bytes and validate against `session.declared_mime` (`cpt-cf-file-storage-fr-content-type-validation`); re-check the policy size ceiling against the resolved MIME; on mismatch fail **before** any DB finalize (the assembled blob becomes an orphan reclaimed by the orphan-reconciliation sweep) - `inst-complete-mime-validate`
9. [x] - `p1` - DB: finalize the version row (`status: pending -> available`, real assembled `size`/composite `content_hash`/`hash_mode`/`part_count`/`manifest`) transactionally; for a **manual** session (`auto_bind = false`) this leaves `content_id` untouched; for an **auto-bind** session (`auto_bind = true`) the same transaction also attempts the `content_id IS NULL` CAS bind (`Store::finalize_version`), touching `content_id` only if it wins - `inst-complete-finalize-version`
10. [x] - `p1` - DB: UPDATE multipart_uploads SET status=completed WHERE upload_id = ? - `inst-complete-db-session`
11. [x] - `p1` - RETURN **200** `{version_id, size, hash_algorithm, content_hash, hash_mode, part_count, manifest, bind_state, etag?, current_etag?}` - `inst-complete-return`
12. [ ] - `p2` - For a **manual** session (`bind_state: "manual"`) or a lost auto-bind CAS (`bind_state: "conflict"`), the client separately calls `POST /files/{id}/bind {version_id}` under `If-Match` to swap `content_id` and make the content live; a won auto-bind CAS (`bind_state: "bound"`) needs no follow-up call - `inst-complete-bind-followup`

### Abort Multipart Upload

- [ ] `p1` - **ID**: `cpt-cf-file-storage-flow-multipart-abort`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Session marked aborted (DB CAS, committed before any backend call is made); part rows and the pending version
  deleted; backend multipart handle best-effort aborted afterward — a backend failure at that point is logged and
  counted via a metric, not returned to the client, since the DB-side abort is already committed and irreversible
  by then

**Error Scenarios**:
- Session already completed or aborted, or the `in_progress -> aborted` CAS loses a race against a concurrent
  complete/abort -- 409 Conflict (the backend is never touched in this case, to avoid corrupting a handle a
  concurrent `complete` may already be assembling from)
- Session not found or client lacks write permission -- 404 / 403

**Steps**:
1. [ ] - `p1` - Client: DELETE /api/file-storage/v1/files/{id}/multipart/{upload_id} - `inst-abort-request`
2. [ ] - `p1` - Authorize per-file `WRITE` on the path `file_id`; load the session by `upload_id` and verify it belongs to `file_id` (a foreign or missing `upload_id` is masked as `404`); RETURN 409 if the session's snapshot state is not `in_progress` - `inst-abort-check-status`
3. [x] - `p1` - DB, one transaction (CAS-first, runs BEFORE any backend call): flip `multipart_uploads.status` `in_progress -> aborted` (or, for a `completing` session whose lease has expired, via `abort_expired_completing`); on success, DELETE FROM `multipart_upload_parts` WHERE `upload_id = ?` and insert the audit row; if the CAS does not win (state already changed under us), RETURN 409 and stop here, before touching the backend or the pending version - `inst-abort-delete-parts`
4. [ ] - `p1` - Best-effort, now that the CAS has won: call backend `AbortMultipart(upload_handle)` to discard backend-side parts. A failure here is logged (`tracing::warn!`) and recorded via `record_backend_error`, then swallowed — never propagated to the client — leaving the backend-side upload for its own garbage collection - `inst-abort-backend`
5. [ ] - `p1` - DB: DELETE the pending version row from `file_versions` WHERE `version_id = ?` AND `status = pending` (a missing row is fine — that is the desired end state) - `inst-abort-delete-version`
6. [ ] - `p1` - RETURN 204 No Content - `inst-abort-return`

### Introspect and Resume Multipart Upload

- [x] `p2` - **ID**: `cpt-cf-file-storage-flow-multipart-introspect`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Client receives the session's current state, the parts already reported (`received`), and the parts still missing
  (`missing`). While the session is still `in_progress` and unexpired, each missing part also carries a freshly
  minted resume `upload_url` so an interrupted upload can continue without re-initiating; a terminal
  (`completed`/`aborted`) or expired session reports state/accounting only, with no URLs

**Error Scenarios**:
- Session not found, or its `upload_id` belongs to a different `file_id` -- `404`-shaped "not found" (masked
  identically to `complete`/`abort`'s foreign-session guard)
- Caller lacks `write` permission on the file -- `403`

**Steps**:
1. [x] - `p1` - Client: GET /api/file-storage/v1/files/{id}/multipart/{upload_id} (no request body) - `inst-introspect-request`
2. [x] - `p1` - Control plane: authorize `write` on `file_id` (same gate as initiate/complete/abort -- introspect hands out live resume URLs, so it is not opened to a read-capable-but-not-write principal) - `inst-introspect-authz`
3. [x] - `p1` - DB: SELECT multipart_uploads WHERE upload_id = ?; verify it belongs to `file_id` -- mask a foreign or missing `upload_id` as `404` identically to `complete`'s guard - `inst-introspect-load-session`
4. [x] - `p1` - DB: SELECT all reported rows from multipart_upload_parts WHERE upload_id = ? - `inst-introspect-load-parts`
5. [x] - `p1` - Diff the plan's expected part numbers against the reported ones using the same `missing_part_numbers` helper (derived from `cpt-cf-file-storage-algo-compute-parts-plan`) that `complete` uses -- `inst-introspect-diff`
6. [x] - `p1` - FOR EACH missing part number: recompute its `(offset, size)` from the session's persisted `(declared_size, part_size)` columns - `inst-introspect-recompute-bounds`
7. [x] - `p1` - **IF** state == in_progress AND expires_at > now: mint a fresh signed part URL for each missing part, reusing the initiate path's per-part token-minting helper, with the token `exp = min(session.expires_at, now + url_ttl_secs)` -- capped at the session's own expiry, but never longer than the standard per-part URL TTL either, so a long-lived session cannot be used to mint effectively-non-expiring resume URLs - `inst-introspect-mint-urls`
8. [x] - `p1` - **ELSE** (terminal or expired session): omit `upload_url` from every missing part - `inst-introspect-no-urls`
9. [x] - `p1` - RETURN 200 {upload_id, version_id, state, declared_mime, declared_size, part_size, created_at, expires_at, received: [{part_number, size, uploaded_at}], missing: [{part_number, offset, size, upload_url?}]} - `inst-introspect-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions that do not interact with actors directly; called by actor flows.

### Compute Parts Plan

- [ ] `p1` - **ID**: `cpt-cf-file-storage-algo-compute-parts-plan`

**Input**: declared_size (uint64), preferred_part_size (uint64 or null), backend.min_part_size (uint64)
**Output**: {part_size, parts: [{part_number, offset, size}], part_hash_algorithm}

`part_hash_algorithm` is always SHA-256, and part sizing has no chunk-tree-boundary constraint. The plan enforces a
hard ceiling of `MAX_PART_COUNT` (10,000) parts, independent of any one backend's own native limit: if the part
count implied by `declared_size` and the computed `part_size` would exceed the ceiling, `part_size` is widened (up to
`MAX_PART_SIZE`, 5 GiB) just enough to bring the plan back under it; if `declared_size` is so large that even
`MAX_PART_SIZE` cannot fit it within `MAX_PART_COUNT` parts, the plan is rejected rather than minted.

**Steps**:
1. [ ] - `p1` - Compute candidate_part_size = max(preferred_part_size ?? backend.min_part_size, backend.min_part_size) - `inst-plan-candidate`
2. [ ] - `p1` - Round candidate_part_size up to backend.min_part_size's granularity - `inst-plan-round`
3. [ ] - `p1` - Compute part_count = ceil(declared_size / part_size) - `inst-plan-count`
4. [ ] - `p1` - FOR EACH i in [1..part_count]: compute offset = (i-1) * part_size; size = min(part_size, declared_size - offset) - `inst-plan-parts`
5. [ ] - `p1` - Set part_hash_algorithm = SHA-256 - `inst-plan-algo-fallback`
6. [ ] - `p1` - RETURN {part_size, parts, part_hash_algorithm} -- the plan is deterministic from (declared_size, part_size) and can be recomputed for resume from the persisted columns - `inst-plan-return`

### Enforce Per-Part Size Claim at Sidecar

- [ ] `p1` - **ID**: `cpt-cf-file-storage-algo-enforce-part-size`

**Input**: request body (stream), size_claim (uint64 from signed token)
**Output**: accepted body bytes, or 413 rejection before any write

**Steps**:
1. [ ] - `p1` - Read Content-Length header from the incoming PUT request - `inst-enforce-read-cl`
2. [ ] - `p1` - **IF** Content-Length is present AND Content-Length != size_claim: RETURN HTTP 413 without buffering or writing any bytes - `inst-enforce-cl-reject`
3. [ ] - `p1` - Stream the body; count bytes as they arrive - `inst-enforce-stream`
4. [ ] - `p1` - **IF** byte count exceeds size_claim before body ends: RETURN HTTP 413 -- abort the write mid-stream; rollback any partially written bytes - `inst-enforce-oversize`
5. [ ] - `p1` - **IF** body ends before size_claim bytes received: RETURN HTTP 400 Bad Request (short body) - `inst-enforce-undersize`
6. [ ] - `p1` - RETURN accepted bytes (exactly size_claim bytes) -- proceed to write - `inst-enforce-accept`

### Combine Part Hashes at Complete

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-combine-part-hashes`

**Input**: ordered list of (part_number, offset, part_hash) from multipart_upload_parts; part_hash_algorithm (always SHA-256)
**Output**: root_hash (hex string), manifest (wire-format text)

`complete` diffs the plan's expected part numbers against the reported ones and rejects with `409` + the missing
part numbers before ever reaching this combination step (see
[Complete Multipart Upload](#complete-multipart-upload)'s `inst-complete-missing-parts`) -- `inst-combine-sort` below
is that same gap check, scoped to this process's own input/output framing. The combination is ADR-0006's
`multipart-composite-sha256` mode (`cpt-cf-file-storage-adr-content-hash-modes`): the backend builds an
offset-manifest from the already-persisted per-part digests+offsets and computes `root_hash = sha256(manifest)` --
**no re-read/re-assembly of the object** happens to compute it.

**Steps**:
1. [x] - `p1` - Sort parts by part_number ascending (the repo's `list_parts` returns `ORDER BY part_number`); verify no gaps in [1..N] via `missing_part_numbers` - `inst-combine-sort`
2. [x] - `p1` - The backend builds the offset-manifest from the per-part digests+offsets and computes `root_hash = sha256(manifest.to_wire_string())` (ADR-0006 `multipart-composite-sha256`) - `inst-combine-sha256`
3. [x] - `p1` - RETURN root_hash + manifest text (persisted in `version_hash_manifest`, returned in the `complete` response body) - `inst-combine-return`

## 4. States (CDSL)

### Multipart Session State Machine

- [ ] `p1` - **ID**: `cpt-cf-file-storage-state-multipart-session`

**States**: in_progress, completed, aborted

**Initial State**: in_progress

**Transitions**:
1. [ ] - `p1` - **FROM** in_progress **TO** completed **WHEN** complete flow verifies all parts and activates the file version - `inst-st-to-completed`
2. [ ] - `p1` - **FROM** in_progress **TO** aborted **WHEN** abort flow is called explicitly by the client - `inst-st-to-aborted`
3. [ ] - `p1` - **FROM** in_progress **TO** aborted **WHEN** TTL/orphan-reconciliation sweep expires an unfinished session (`cpt-cf-file-storage-fr-orphan-reconciliation`) - `inst-st-ttl-abort`

## 5. Definitions of Done

### Initiate Endpoint with Server-Authoritative Plan

- [ ] `p1` - **ID**: `cpt-cf-file-storage-dod-multipart-initiate`

The system **MUST** implement `POST /api/file-storage/v1/files/{id}/multipart` on the control plane. The endpoint validates declared_mime, declared_size, and storage quota; calls `cpt-cf-file-storage-algo-compute-parts-plan`; pre-registers a pending version; persists the multipart session with declared_size and part_size; mints one signed URL per part (Ed25519, codec-equivalent to PASETO v4.public -- ADR-0004) (claims: upload_id, file_id, version_id, part_number, offset, size, op, exp); and returns the full parts plan. The parts plan enforces a hard ceiling of 10,000 parts (`MAX_PART_COUNT`), widening the part size (up to `MAX_PART_SIZE`, 5 GiB) before rejecting a `declared_size` that cannot fit even at the maximum part size.

**Implements**:
- `cpt-cf-file-storage-flow-multipart-initiate`
- `cpt-cf-file-storage-algo-compute-parts-plan`

**Touches**:
- API: `POST /api/file-storage/v1/files/{id}/multipart`
- DB Table: `multipart_uploads`
- DB Table: `file_versions`

### Sidecar Per-Part Enforcement

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-multipart-sidecar-enforcement`

The sidecar part-upload handler verifies the signed token (Ed25519, codec-equivalent to but not literal
PASETO -- ADR-0004's Implementation note); calls `cpt-cf-file-storage-algo-enforce-part-size` to reject with HTTP 413
if the body length does not match the size claim (enforced as a streaming `max_size` abort plus a post-write
exact-length check, not a pre-write `Content-Length` check); writes the part bytes to the backend (`PutPart` for
`multipart_native`, or a separate `{backend_path}.part.{n}` object for non-native backends -- not an offset-write into
the shared version object); computes the per-part hash; and reports it to the control plane over a token-authenticated
callback, which upserts the part row (the sidecar itself never touches the DB). Re-PUT of the same (upload_id,
part_number) is idempotent.

**Implements**:
- `cpt-cf-file-storage-flow-multipart-upload-part`
- `cpt-cf-file-storage-algo-enforce-part-size`

**Touches**:
- API: `PUT <sidecar signed part URL>`
- DB Table: `multipart_upload_parts`

### Complete Endpoint with Hash Combination

- [x] `p1` - **ID**: `cpt-cf-file-storage-dod-multipart-complete`

`POST /api/file-storage/v1/files/{id}/multipart/{upload_id}/complete` diffs the plan's expected part
numbers against the reported ones and rejects with `409` + the missing part numbers if any are absent;
then verifies `SUM(reported part sizes) == declared_size` as a residual guard (generic `409` on mismatch); asks the
backend to build the ADR-0006 offset-manifest and compute its composite root hash from the already-persisted
per-part digests (no re-read of the assembled object); **finalizes** the version (`pending -> available`) with the
real size + composite hash/mode/part-count/manifest; marks the session completed; returns **`200`** with
`{version_id, size, hash_algorithm, content_hash, hash_mode, part_count, manifest, bind_state, etag?,
current_etag?}`. It accepts an **optional**
`If-Match` header (a concrete value is checked against the file's current content ETag, `*`/absent is unconditional).
Whether it also binds the version depends on the session's `auto_bind` flag, fixed at initiate time: a **manual**
session (`bind_state: "manual"`) does **not bind** -- binding is a separate, later client-issued
`POST /files/{id}/bind`; an **auto-bind** session binds the version itself, in the same transaction as finalize,
under a `content_id IS NULL` CAS (`bind_state: "bound"` on a won CAS, `"conflict"` on a lost one, with no separate
bind call needed in the `"bound"` case).

**Implements**:
- `cpt-cf-file-storage-flow-multipart-complete`
- `cpt-cf-file-storage-algo-combine-part-hashes` (ADR-0006 manifest-composite path, `inst-combine-sort`/`inst-combine-sha256`/`inst-combine-return`)

**Touches**:
- API: `POST /api/file-storage/v1/files/{id}/multipart/{upload_id}/complete`
- DB Table: `multipart_uploads`
- DB Table: `multipart_upload_parts`
- DB Table: `file_versions`

### Abort Endpoint

- [ ] `p1` - **ID**: `cpt-cf-file-storage-dod-multipart-abort`

The system **MUST** implement `DELETE /api/file-storage/v1/files/{id}/multipart/{upload_id}`: verify session is
in_progress; win the DB CAS marking the session aborted and deleting part rows (in one transaction) BEFORE touching
the backend; then best-effort abort the backend handle (multipart_native only) -- a backend failure at this point is
logged and metered, not returned to the client, since the DB-side abort already committed; then delete the pending
version.

**Implements**:
- `cpt-cf-file-storage-flow-multipart-abort`

**Touches**:
- API: `DELETE /api/file-storage/v1/files/{id}/multipart/{upload_id}`
- DB Table: `multipart_uploads`
- DB Table: `multipart_upload_parts`
- DB Table: `file_versions`

### Introspect and Resume Endpoint

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-multipart-introspect`

`GET /api/file-storage/v1/files/{id}/multipart/{upload_id}` returns the
session's current state, the parts already reported (from `multipart_upload_parts`), and the parts still missing
(their `(offset, size)` recomputed from the session's persisted `declared_size`/`part_size` columns via the same
`missing_part_numbers` helper `complete` uses). While the session is still `in_progress` and
unexpired, each missing part also gets a freshly-minted resume URL, reusing the initiate path's per-part
token-minting helper, with `exp = min(session.expires_at, now + url_ttl_secs)`: capped so a resumed upload can never
outlive the session it resumes, but also never minted with a longer TTL than any other signed URL just because the
session itself is long-lived (`multipart_session_ttl_secs`, e.g. 24h, is deliberately decoupled from the short
per-part URL TTL -- see `operations.md`'s `multipart_session_ttl_secs` entry). A terminal (`completed`/`aborted`) or expired session reports state and
part accounting only, with no URLs. Authorized on `actions::WRITE` (not `READ`), matching initiate/complete/abort --
introspect hands out live upload URLs, so it is not opened to a read-capable-but-not-write principal. A foreign or
missing `upload_id` is masked as `404`, identical to `complete`'s guard.

**Implements**:
- `cpt-cf-file-storage-flow-multipart-introspect`
- `cpt-cf-file-storage-flow-multipart-initiate`
- `cpt-cf-file-storage-algo-compute-parts-plan`

**Touches**:
- API: `GET /api/file-storage/v1/files/{id}/multipart/{upload_id}`
- DB Table: `multipart_uploads`
- DB Table: `multipart_upload_parts`

### Schema: multipart_uploads Plan Columns

- [ ] `p1` - **ID**: `cpt-cf-file-storage-dod-multipart-schema-plan-columns`

The system **MUST** add `version_id uuid NOT NULL`, `declared_size bigint NOT NULL CHECK (declared_size >= 0)`, and `part_size bigint NOT NULL` to the `multipart_uploads` table via migration `m20260701_000002_multipart_plan_columns`. These three columns make the plan deterministic from the session row (no per-part plan table needed), enable complete-time size verification without re-summing parts, and allow the introspect endpoint to reconstruct the plan for resume.

**Implements**:
- `cpt-cf-file-storage-flow-multipart-initiate`
- `cpt-cf-file-storage-flow-multipart-complete`
- `cpt-cf-file-storage-algo-compute-parts-plan`

**Touches**:
- DB Table: `multipart_uploads`

## 6. Acceptance Criteria

- [x] `POST /api/file-storage/v1/files/{id}/multipart` returns a parts plan with one signed sidecar URL per part; each URL token includes part_number, offset, size, op, and exp claims
- [x] The parts plan is server-computed from declared_size and the effective part_size; clients cannot choose part boundaries
- [x] The control-plane route `PUT /files/{id}/multipart/{upload_id}/parts/{part_number}` does not exist; all part bytes flow through sidecar signed URLs only (ADR-0003)
- [x] The sidecar rejects a PUT whose body length does not match the size claim in the token with HTTP 413 before writing any bytes
- [x] Re-PUT of the same (upload_id, part_number) is idempotent; the part row is overwritten and no duplicate rows are created
- [x] `POST .../complete` rejects with `409 Conflict` and the missing part numbers in the error detail when one or
  more planned parts have not been reported yet; a residual generic `409` still guards
  `SUM(reported part sizes) != declared_size` for cases the missing-parts diff cannot catch (e.g. an over-reported part)
- [x] `POST .../complete` **finalizes** the file version (`pending -> available`) with the real assembled size and
  ADR-0006 composite hash, and returns **`200`** with `{version_id, size, hash_algorithm, content_hash, hash_mode,
  part_count, manifest, bind_state, etag?, current_etag?}`; session status becomes completed. Whether it also
  binds the version depends on the session's `auto_bind` flag: a **manual** session leaves `content_id` untouched
  (`bind_state: "manual"`); an **auto-bind** session binds it in the same transaction as finalize under a
  `content_id IS NULL` CAS (`bind_state: "bound"` on a won CAS, `"conflict"` on a lost one). It accepts an
  **optional** `If-Match` header (checked against the file's current content ETag; `400`
  on mismatch, `FailedPrecondition` collapses to `400` on this platform -- there is no `412`-mapped canonical-error
  variant; `*`/absent is unconditional)
- [x] `DELETE .../multipart/{upload_id}` wins a DB CAS marking the session aborted and deletes part rows (one
  transaction, before any backend call); deletes the pending version; then best-effort aborts the backend handle for
  multipart_native backends -- a backend abort failure at that point is logged and metered rather than returned to
  the client, since the DB-side abort is already committed
- [x] Initiating a multipart upload with declared_size exceeding the effective size-limit policy returns 413; exceeding storage quota returns 429; unsupported MIME returns 415
  (this `[x]` is exercised only by `tests/enforce_test.rs` injecting a mock `QuotaClient` — no real deployment has
  one wired (`gear.rs`'s `quota_client: None`), so this rejection path does not fire in production; see
  `../operations.md#storage-quota-not-enforced`)
- [x] multipart_uploads rows carry version_id, declared_size, and part_size columns (migration m20260701_000002_multipart_plan_columns)
- [x] Against the real default topology (`local-fs`, not `multipart_native`), `POST /files/{id}/multipart` is rejected
  (see the caveat in §1.2) -- multipart is only functional against a `multipart_native` backend
  (`InMemoryBackend`, dev/test only, or the opt-in `S3Backend`)
- [x] `POST .../complete` accepts `If-Match` and returns `400` (`FailedPrecondition`; this platform has no `412`
  canonical-error variant) on a stale precondition, directly (not via a separate `bind` call)
- [x] `GET /files/{id}/multipart/{upload_id}` (introspect) returns the session state plus `received`/`missing` part
  lists; missing parts of a still-`in_progress`, unexpired session each carry a fresh resume `upload_url` whose
  `exp = min(session.expires_at, now + url_ttl_secs)` (never longer than the standard per-part URL TTL, even though
  the session itself may still have much longer left to live); a terminal or expired session omits the URLs; a
  foreign or missing `upload_id` is masked as `404`
- [x] `POST .../complete` returns `200 {version_id, size, hash_algorithm, content_hash, hash_mode, part_count, manifest}`
  instead of `204`
- [x] `POST .../complete` enumerates missing part numbers in its `409` body instead of a bare size comparison
- [ ] Non-native backends write parts as offset-writes into the shared version object instead of per-part objects
- [ ] `GET .../multipart/{upload_id}` returns the plan recomputed from persisted columns and re-issues fresh signed URLs for missing parts
