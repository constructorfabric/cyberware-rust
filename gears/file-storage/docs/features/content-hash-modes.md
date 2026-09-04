Created:  2026-07-07 by Constructor Tech
Updated:  2026-07-07 by Constructor Tech
# Feature: Content-Hash Modes

- [ ] `p2` - **ID**: `cpt-cf-file-storage-featstatus-content-hash-modes-implemented`



<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Client-Side Manifest Re-Verification](#client-side-manifest-re-verification)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Build Offset-Manifest at Complete](#build-offset-manifest-at-complete)
  - [Mode-Aware Content-Hash Verification](#mode-aware-content-hash-verification)
- [4. States (CDSL)](#4-states-cdsl)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Groundwork — Hash-Mode Types & Manifest Wire Format](#groundwork--hash-mode-types--manifest-wire-format)
  - [Schema Migration — hash_mode, part_count, version_hash_manifest](#schema-migration--hash_mode-part_count-version_hash_manifest)
  - [Multipart-Composite-SHA-256 Implementation](#multipart-composite-sha-256-implementation)
  - [Documentation Updates](#documentation-updates)
- [6. Acceptance Criteria](#6-acceptance-criteria)
- [7. Detailed Design Reference](#7-detailed-design-reference)
  - [1. Current state](#1-current-state)
  - [2. The two modes](#2-the-two-modes)
  - [3. Manifest encoding — canonical wire format](#3-manifest-encoding--canonical-wire-format)
  - [4. Manifest storage](#4-manifest-storage)
  - [5. Schema changes](#5-schema-changes)
  - [6. Verification / recompute paths](#6-verification--recompute-paths)
  - [7. Trait changes](#7-trait-changes)
  - [8. Cross-backend mapping](#8-cross-backend-mapping)
  - [9. Mode selection](#9-mode-selection)
  - [10. FIPS handling](#10-fips-handling)
  - [11. Risks & open decisions](#11-risks--open-decisions)

<!-- /toc -->

## 1. Feature Context

- [ ] `p2` - `cpt-cf-file-storage-feature-content-hash-modes`

### 1.1 Overview

Exactly two content-hash modes exist for file-storage, both SHA-256, distinguished only by upload path
(non-multipart vs. multipart), never by user or operator choice: (1) non-multipart uploads use the plain
`sha256(whole object bytes)`; (2) multipart uploads of **two or more parts** use a **SHA-256 offset-manifest
composite** — a canonical manifest recording each part's byte offset and `sha256(part_bytes)`, with the stored digest
being `root = sha256(manifest)` — built entirely from already-computed per-part digests, with no re-read of the
assembled object at `complete` time (ADR-0006, `status: accepted`). A multipart plan of exactly **one part**
degenerates to `whole-sha256` (ADR-0006 single-part amendment): the single part's streaming digest is already the
whole-object digest, so no composite and no manifest are produced.

### 1.2 Purpose

Multipart `complete` derives the version's content hash entirely from the per-part SHA-256 digests already
persisted in `multipart_upload_parts`, with no re-read/re-assembly of the object at `complete` time -- avoiding the
full extra read pass (doubled egress/bandwidth) a re-read would otherwise cost on every completed multipart upload.
The resulting composite digest is also **independently client-verifiable** from the object bytes plus the small,
durable manifest the API returns, with no dependency on retaining multipart-session state. This is formalized in
[ADR-0006](../ADR/0006-cpt-cf-file-storage-adr-content-hash-modes.md) (`status: accepted`), which supersedes
[ADR-0002](../ADR/0002-cpt-cf-file-storage-adr-content-hash-selection.md)'s `hash_policy`/selection-rules vision for
the content-hash-modes decision specifically: that vision is out of scope entirely, not merely deferred, because
SHA-256 is the only algorithm for both modes and there is no remaining choice to configure or discover.

**Requirements**: `cpt-cf-file-storage-fr-multipart-upload`, `cpt-cf-file-storage-fr-metadata-storage`,
`cpt-cf-file-storage-fr-get-metadata`

**Principles**: `cpt-cf-file-storage-principle-streaming`, `cpt-cf-file-storage-principle-control-no-content`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-file-storage-actor-platform-user` | Receives `hash_mode`/`part_count` in both the `complete` response and version-metadata (`GET /files/{id}/versions`) responses; the `manifest` text is returned by the `complete` response **and** re-served on composite versions by `GET /files/{id}/versions` (`VersionDto.manifest`), so the stored `root` can be re-derived and verified from the object bytes plus the manifest at any time. One-part plans finalize as `whole-sha256` and carry no manifest at all (ADR-0006 single-part amendment) |
| `cpt-cf-file-storage-actor-cf-gears` | Peer gear / service consuming the same additive metadata fields; also the actor that triggers `migrate_backend`, whose mode-aware verification this feature makes self-contained |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md) — §"Multipart upload — P2" and the hash/ETag pipeline section (corrected --
  see §7 below)
- **ADR**: [ADR-0006](../ADR/0006-cpt-cf-file-storage-adr-content-hash-modes.md) — Content-hash modes decision record
  this feature implements (`status: accepted`, implemented)
- **ADR**: [ADR-0002](../ADR/0002-cpt-cf-file-storage-adr-content-hash-selection.md) — Content hash selection; its
  `hash_policy`/selection-rules vision is superseded by ADR-0006 for this decision specifically
- **Dependencies**: [Multipart Upload Coordinator](multipart-coordinator.md)
  (`cpt-cf-file-storage-feature-multipart-coordinator`) — this feature builds on the multipart session/part-hash
  plumbing that feature already ships (`multipart_upload_parts.part_hash`, `compute_plan`'s per-part offsets); it does
  not change that feature's endpoints, only how `complete_multipart_upload` derives the stored content hash

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a
use case. This feature introduces no new endpoint (`ARCH-FDESIGN-NO-002`): the existing
`POST .../multipart/{upload_id}/complete` and metadata-fetch routes (owned by
`cpt-cf-file-storage-feature-multipart-coordinator` and the P1 foundation, respectively) are unchanged in shape —
only the `hash_mode`/`part_count`/`manifest` fields they surface are new, additive, non-breaking wire-format content.
The one genuinely new actor-facing journey this feature enables is independent client-side re-verification.

### Client-Side Manifest Re-Verification

- [ ] `p2` - **ID**: `cpt-cf-file-storage-flow-content-hash-modes-client-reverify`

**Actor**: `cpt-cf-file-storage-actor-platform-user`

**Success Scenarios**:
- Client holding the full object bytes plus the `root` and `manifest` it retained from the multipart `complete`
  response independently re-derives `root` using nothing but a stock SHA-256 implementation and confirms it matches,
  with no dependency on any multipart-session state or trust in the server's claimed offsets alone

**Error Scenarios**:
- Any byte in any part has been tampered with or corrupted — the recomputed per-part digest does not match the
  manifest's recorded digest for that slice, or the rebuilt manifest's hash does not equal `root`
- Client attempts re-verification for a `whole-sha256` version using the multipart algorithm — rejected as a caller
  error (`whole-sha256` versions carry no manifest; re-verification is a direct `sha256(object bytes)` comparison)

**Steps**:
1. [ ] - `p2` - Client: fetch version metadata (`GET /files/{id}/versions`), obtaining `hash_mode`, the stored hash
   (wire field `hash` on `VersionDto`; the `root` for a composite version — stored as `hash_value` in the DB),
   `part_count`, and — for a `multipart-composite-sha256` version — the `manifest` wire text itself (`VersionDto`
   now carries a `manifest` field, re-served from the stored `version_hash_manifest` row; the same text the original
   `POST .../complete` response returned as `content_hash`'s companion, so retaining that response is no longer the
   only way to re-verify)
   - `inst-reverify-fetch-metadata`
2. [ ] - `p2` - **IF** `hash_mode == whole-sha256`: compute `sha256(object bytes)` directly and compare to
   the fetched hash (`VersionDto.hash`) - `inst-reverify-whole`
3. [ ] - `p2` - **ELSE** (`hash_mode == multipart-composite-sha256`): Algorithm: re-derive and verify using
   `cpt-cf-file-storage-algo-content-hash-modes-verify` - `inst-reverify-composite`
4. [ ] - `p2` - **RETURN** verification result (match / mismatch, with the specific diverging part offset when
   possible) to the caller - `inst-reverify-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions that do not interact with actors directly; called by actor flows and by the existing
`complete_multipart_upload` / `migrate_backend` control-plane paths owned by other features.

### Build Offset-Manifest at Complete

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-content-hash-modes-build-manifest`

**Input**: ordered list of `(part_number, offset, part_hash)` from `multipart_upload_parts` (already collected
during upload; `offset` from the multipart plan's `compute_plan` output, `part_hash = sha256(part_bytes)` already
computed per-part)

**Output**: `(manifest: Manifest, root: [u8; 32])`

**Steps**:
1. [x] - `p1` - Sort entries by ascending byte offset (identical to ascending part-number order for any valid plan) - `inst-buildmanifest-sort`
2. [x] - `p1` - FOR EACH entry: serialize as `{offset}:{64-lowercase-hex-digest}` per the canonical grammar (§3 below) - `inst-buildmanifest-serialize-entry`
3. [x] - `p1` - Concatenate entries with `,` separators, prefixed by the `v1,` version token, with no trailing delimiter and no whitespace - `inst-buildmanifest-concat`
4. [x] - `p1` - Compute `root = sha256(manifest_bytes)` where `manifest_bytes` is the manifest string's UTF-8 encoding - `inst-buildmanifest-root`
5. [x] - `p1` - **RETURN** `(manifest, root)` — no `GetObject`/re-read of the assembled object occurs at any step - `inst-buildmanifest-return`

### Mode-Aware Content-Hash Verification

- [x] `p1` - **ID**: `cpt-cf-file-storage-algo-content-hash-modes-verify`

**Input**: `blob` (object bytes), `hash_mode`, `hash_value`, `manifest: Option<&str>` (required when
`hash_mode == multipart-composite-sha256`, absent otherwise)

**Output**: `Ok(())` or a hash-mismatch error

**Steps**:
1. [x] - `p1` - **IF** `hash_mode == whole-sha256`: compute `sha256(blob)`; compare to `hash_value`; RETURN mismatch error if unequal - `inst-verify-whole`
2. [x] - `p1` - **ELSE** (`hash_mode == multipart-composite-sha256`, `manifest` MUST be present): parse `manifest` into ordered `(offset, digest)` entries - `inst-verify-parse-manifest`
3. [x] - `p1` - FOR EACH parsed entry: slice `blob` at `offset` (final entry's length derives from the object's known `size`); compute `sha256` of the slice; compare to the entry's recorded digest; RETURN mismatch error (naming the diverging offset) if unequal - `inst-verify-per-part`
4. [x] - `p1` - Re-serialize the manifest from the recomputed digests using the exact grammar in §3 below - `inst-verify-reserialize`
5. [x] - `p1` - Compute `sha256(reserialized_manifest)`; compare to `hash_value` (`root`); RETURN mismatch error if unequal - `inst-verify-root-compare`
6. [x] - `p1` - **RETURN** `Ok(())` - `inst-verify-return`

This single algorithm is shared by three call sites: the client-side flow above, the control plane's
`Store::verify_content_hash` (used by single-part finalize), and `migrate_backend`'s destination-write verification —
none of them re-derive the split/rebuild/compare sequence independently.

## 4. States (CDSL)

**Not applicable.** `hash_mode` is a per-version attribute set exactly once, at finalize time, from which code path
executed (non-multipart vs. multipart) — it is not a stateful entity with transitions, guards, or reachable/invalid
states of its own. The multipart session's own lifecycle (`in_progress` → `completed`/`aborted`) is unchanged by this
feature and remains owned by `cpt-cf-file-storage-feature-multipart-coordinator`'s
`cpt-cf-file-storage-state-multipart-session` state machine.

## 5. Definitions of Done

### Groundwork — Hash-Mode Types & Manifest Wire Format

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-content-hash-modes-groundwork`

The system **MUST** introduce `HashMode`, `ManifestEntry`, and `Manifest` types (`src/infra/content/hash_mode.rs`,
alongside the existing `hash.rs`), with `Manifest::to_wire_string()`/`from_wire_string()` implementing the canonical
grammar in §3 (below) exactly, plus round-trip and cross-implementation-stability unit tests confirming
`to_wire_string()` output matches a hand-computed expected string and `sha256(to_wire_string(...))` matches an
independently-computed reference `root`.

**Implements**:
- `cpt-cf-file-storage-algo-content-hash-modes-build-manifest`

**Touches**:
- Gears: `src/infra/content/hash_mode.rs` (new)

### Schema Migration — hash_mode, part_count, version_hash_manifest

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-content-hash-modes-schema`

The system **MUST** add `hash_mode` (`'whole-sha256'` | `'multipart-composite-sha256'`, default `'whole-sha256'`) and
`part_count` (`NOT NULL` only for the multipart mode) to `file_versions`, plus a new `version_hash_manifest` table
(`version_id` PK/FK into `file_versions`, `manifest text NOT NULL`, `created_at`). Existing rows backfill to
`hash_mode = 'whole-sha256'`, `part_count = NULL`, no `version_hash_manifest` row — correct, since every extant row
is a P1 single-part SHA-256 upload requiring no re-hash. The existing `hash_algorithm` CHECK
(`= 'SHA-256'`) is left untouched — never widened, since both modes use SHA-256 as their only underlying primitive.
`VersionRepo::finalize`/`Store::finalize_version` gain `hash_mode`/`part_count` parameters, set at **finalize** time
(mirroring the existing gap where `hash_algorithm` is fixed at pending-insert time but a pending row cannot yet know
whether the upload will end up single- or multi-part).

**Implements**:
- `cpt-cf-file-storage-algo-content-hash-modes-build-manifest`

**Touches**:
- DB Table: `file_versions`
- DB Table: `version_hash_manifest` (new)

### Multipart-Composite-SHA-256 Implementation

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-content-hash-modes-multipart-composite`

The system **MUST** widen `StorageBackend::upload_part`/`complete_multipart` (§7 below) so every multipart-capable
backend (`S3Backend`, `InMemoryBackend`) builds the manifest and its `root` from the already-collected
`(offset, part_hash)` pairs instead of re-reading the assembled object; `MultipartService::complete_multipart_upload`
must stop discarding parts' hashes/offsets and persist the returned `Manifest` into `version_hash_manifest`
transactionally with `finalize_version`. `Store::verify_content_hash` and `migrate_backend` **MUST** become
mode-aware per `cpt-cf-file-storage-algo-content-hash-modes-verify`.

**Implements**:
- `cpt-cf-file-storage-flow-content-hash-modes-client-reverify`
- `cpt-cf-file-storage-algo-content-hash-modes-build-manifest`
- `cpt-cf-file-storage-algo-content-hash-modes-verify`

**Touches**:
- API: `POST /api/file-storage/v1/files/{id}/multipart/{upload_id}/complete` (response fields only — no shape/method change)
- DB Table: `multipart_upload_parts`
- DB Table: `version_hash_manifest`
- DB Table: `file_versions`

### Documentation Updates

- [x] `p2` - **ID**: `cpt-cf-file-storage-dod-content-hash-modes-docs`

The system **MUST** correct `DESIGN.md`'s stale hash-design passages and `docs/api.md`'s metadata/upload response
shapes to surface `hash_mode`, `part_count`, and (for multipart-composite versions) `manifest` once implemented;
`SECURITY.md` requires no addendum under this design (§10 below — SHA-256 only, nothing FIPS-relevant to add).

**Implements**:
- `cpt-cf-file-storage-algo-content-hash-modes-build-manifest`

**Touches**:
- Docs: `DESIGN.md`, `api.md`

## 6. Acceptance Criteria

- [x] The manifest wire format round-trips byte-identically: a fixed set of `(offset, digest)` pairs always
  serializes to the same expected string, and `sha256` of that string matches an independently-computed reference `root`
- [x] `complete_multipart` for a `multipart-composite-sha256` version issues no `GetObject`/re-read of the assembled
  object to compute the hash — verified by a request-counting wrapper backend or S3-mock call-count assertion. (The
  control-plane `complete` endpoint separately performs one small ~8 KiB ranged read of the assembled object for
  post-assembly MIME sniffing, unrelated to hash computation; see `multipart-coordinator.md`'s Complete Multipart
  Upload flow, post-assembly MIME-sniff step.)
- [x] A client-side re-verification helper (split the object at the manifest's offsets, rehash, rebuild, compare to
  `root`) succeeds against real uploaded content and fails when any byte in any part is tampered with
- [x] `migrate_backend` verifies a `multipart-composite-sha256` version using only the object bytes and the stored
  `version_hash_manifest` row, with no dependency on `multipart_upload_parts` surviving past the multipart session's
  own lifecycle
- [x] `hash_algorithm`'s `CHECK (hash_algorithm = 'SHA-256')` is unchanged; no second hash algorithm, Cargo dependency,
  or FIPS feature gate is introduced anywhere in the implementation
- [x] Every pre-existing `file_versions` row backfills to `hash_mode = 'whole-sha256'`, `part_count = NULL`, with no
  data migration/re-hashing and no `version_hash_manifest` row
- [x] The finalize-time `expected_hash` client-claim check continues to reject a mismatched client-supplied hash for
  the `whole-sha256` mode, unchanged

## 7. Detailed Design Reference

This design is formalized as a decision record in
[ADR-0006](../ADR/0006-cpt-cf-file-storage-adr-content-hash-modes.md)
(`status: accepted`); ADR-0006 supersedes ADR-0002's
vision for the content-hash-modes decision specifically.
Scope: `gears/file-storage/file-storage` (control plane, sidecar, `StorageBackend`
trait, DB schema).

Goal, per maintainer decision: exactly **2** content-hash modes, both SHA-256,
distinguished only by upload path (non-multipart vs. multipart), never by
user/config choice. The multipart mode is a bespoke **SHA-256 offset-manifest
composite** — not a flat digest concatenation and not a canonical whole-object
hash — chosen for **explicit, arbitrary-part-size, independently
client-verifiable re-derivation**, with no power-of-two part-size constraint
and no second hash algorithm to gate behind a FIPS feature flag —

1. **Non-multipart** upload → plain **`sha256(whole object bytes)`** — the
   canonical whole-object hash, unchanged. Not represented as a
   1-part manifest; there is no manifest at all for this mode.
2. **Multipart** upload → **SHA-256 offset-manifest composite**: per part,
   `sha256(part_bytes)` computed on-the-fly during that part's upload (already
   done); a canonical **manifest** string records, for every part in
   ascending order, `{byte_offset}:{hex(sha256(part))}`; the stored digest is
   `root = sha256(manifest)`. Both `root` and `manifest` are stored and
   returned by the API, so a client holding the file bytes can independently
   re-verify with cryptographic precision — split at the recorded offsets,
   hash each part, rebuild the manifest, compare `sha256(manifest)` to `root`
   — with no dependency on retaining any multipart-session state.

This document is intentionally blunt about trade-offs: mode 2's digest is a
hash of "content *and* split layout," not of the object's bytes alone, so it
is **not** comparable to a whole-object SHA-256 of the same content. That is
an accepted, explicitly documented consequence (§11), not an oversight.

### 1. Current state

**Hashing.** `src/infra/content/hash.rs` is the gear's single SHA-256 call
site (Dylint `DE0708` allow-list entry). `ALGORITHM = "SHA-256"` is a
constant; `sha256`, `sha256_parts`, the incremental `Hasher`, and
`digest_to_array` (panics if the digest isn't 32 bytes) are all SHA-256-only.
Both hash modes use this one primitive, so `hash.rs` stays the gear's only
hash-primitive call site — no second dependency, Cargo feature gate, or FIPS
carve-out beyond what already exists.

**`StorageBackend` trait** (`src/infra/backend/mod.rs`):
- `upload_part` returns `(backend_etag, part_hash)` where `part_hash =
  hash::sha256(&data)` — a flat SHA-256 of that part's bytes — and also
  threads the part's byte offset through so `complete_multipart` can record
  it in the manifest.
- `complete_multipart` takes the ordered `(part_number, offset, part_hash,
  backend_etag)` tuples the caller already collected during upload and
  returns `(Manifest, [u8; 32])` — the manifest and its `root`, built by the
  shared `build_manifest_and_root` helper (`src/infra/backend/mod.rs`).
  Neither multipart-capable backend re-reads the assembled object to compute
  this:
  - `S3Backend::complete_multipart` (`src/infra/backend/s3.rs`) POSTs
    `CompleteMultipartUpload` using the parts' backend `ETag`s, then builds
    the manifest/root from the already-collected `(offset, part_hash)` pairs
    — no `GetObject` re-read.
  - `InMemoryBackend::complete_multipart` (`src/infra/backend/in_memory.rs`)
    assembles the parts into the blob store (so `get` still returns the
    whole object) but likewise derives the stored hash from the
    already-collected per-part digests, not by re-hashing the assembled
    bytes.
  - `LocalFsBackend` has **no multipart support at all** — it inherits the
    trait's default `Err(multipart_not_supported)` for `initiate_multipart`/
    `upload_part`/`complete_multipart`/`abort_multipart`.
- `report_part` / `MultipartStore::upsert_multipart_part` persist each part's
  `part_hash` into `multipart_upload_parts.part_hash` (`bytea`, **no length
  CHECK** — `src/infra/storage/entity/multipart_upload_part.rs`).
  `complete_multipart_upload` reads these rows back (`list_multipart_parts`)
  and threads each part's offset and digest into `complete_multipart` — the
  per-part hash is not write-only.

**Multipart control flow**
(`src/domain/multipart_service.rs::complete_multipart_upload`): loads
`parts` from the store, builds `backend_parts: Vec<(u32, u64, [u8; 32],
String)>` carrying every part's number, offset, SHA-256 digest, and backend
`ETag`; calls `backend.complete_multipart(...)`; and persists the returned
manifest root as `hash_value` plus the manifest text (in
`version_hash_manifest`) via `store.finalize_version(...)`.

**Single-part finalize** (`src/domain/service/write.rs::finalize_upload` /
`finalize_upload_by_token`): both stream the blob back from the backend via
`get_stream`, recompute SHA-256 incrementally with `hash::Hasher`, and
`hash_mismatch` on any divergence from the client-claimed digest. Never
trusts the caller. This read-back is retained for mode 1 -- only the
multipart (mode 2) path avoids re-reading the assembled object.

**`migrate_backend`** (`src/domain/service/backend.rs`): reads the whole blob
from the source backend, then calls `Store::verify_content_hash(&bytes,
hash_mode, &version.hash_value, manifest)` -- mode-aware: for `whole-sha256`
it hashes the blob directly and compares; for `multipart-composite-sha256`
it fetches the version's `version_hash_manifest` row, splits the blob at the
manifest's recorded offsets, rebuilds the manifest from the recomputed
per-part digests, and compares its hash to `hash_value` -- before writing to
the destination.

**Schema** (`m20260624_000001_p1_initial.rs`):
```sql
hash_algorithm text NOT NULL DEFAULT 'SHA-256' CHECK (hash_algorithm = 'SHA-256'),
hash_value     bytea NOT NULL CHECK (octet_length(hash_value) = 32),
```
This CHECK is unchanged by the later migrations (`m20260701_000001_p2_initial.rs`,
`m20260701_000002_multipart_plan_columns.rs`, `m20260707_000001_content_hash_modes.rs`
— the last of which adds `hash_mode`/`part_count` to `file_versions` and the new
`version_hash_manifest` table, §5). `hash_algorithm` is written **once**, at
`pending_version` insert time (`store/mod.rs::pending_version`, hard-coded to
`hash::ALGORITHM`), and `VersionRepo::finalize` never touches it -- only `size`,
`hash_value`, `status`, `mime_type`, `hash_mode`, `part_count` are updated at
finalize. `hash_algorithm` never needs a second value (it stays `'SHA-256'`
forever, §5); `hash_mode` is the discriminator that distinguishes "this
`hash_value` is a whole-object hash" from "this `hash_value` is a manifest
root," and -- like `hash_algorithm` before it -- is only knowable at
**finalize** time, since a pending row is created before it is known whether
the upload will end up single-part or multipart.

**ADR-0002** ("Content Integrity Hash — SHA-256 in P1, Configurable in P2")
describes a rich P2 design: per-backend `hash_policy` (`default_algorithm`,
`allowed_algorithms`, `selection_rules`), a client-preference parameter, and a
capability-discovery endpoint. **None of this is implemented**, and none of it
is what this design builds: there is no algorithm choice left to configure or
discover (SHA-256 is the only algorithm, always), so ADR-0002's
`hash_policy`/`selection_rules`/discovery-endpoint surface is fully out of
scope, not merely deferred.

**FIPS** (repo-root `docs/security/SECURITY.md` §9, `deny-fips.toml`):
Dylint lint `DE0708` bans new `sha2`/`sha1`/`md5` imports outside a one-entry
allow-list (this gear's `hash.rs`). Both hash modes use only that one
primitive, so none of `deny-fips.toml`'s machinery, Cargo feature gating, or
FIPS carve-out reasoning is relevant here — see §10.

### 2. The two modes

A new discriminator is needed everywhere "SHA-256, whole-object" is currently
implicit. Wire/storage value: `hash_mode` ∈ `whole-sha256` | `multipart-composite-sha256`.
`hash_algorithm` stays exactly `'SHA-256'` for both — there is no per-mode
algorithm choice, only a per-mode *shape* difference in what `hash_value`
means.

**On-the-fly principle (rule 4).** Every mode
below is computed **as bytes transit the sidecar during upload** — never by
re-reading the stored object afterward. This is the fourth rule of the
design, alongside the two modes below, and it applies identically to
both: mode 1's whole-object hash is the direct streaming tap it always was;
mode 2's manifest and root are built entirely from already-computed per-part
digests, no re-read of the assembled object required.

#### Mode 1 — non-multipart, whole-object SHA-256

- **Per-part computation**: N/A (single stream).
- **Complete computation**: unchanged — `hash::Hasher`-style streaming
  accumulator over every chunk as it transits the sidecar
  (`put_stream`/`write_stream_to_tmp`/`read_back_and_hash_streaming`).
- **Stored fields**: `hash_algorithm = 'SHA-256'`, `hash_mode =
  'whole-sha256'`, `hash_value` = 32-byte whole-object SHA-256 digest. No
  manifest row for this mode — it is **not** represented as a 1-part
  manifest; there is nothing to reconstruct beyond re-hashing the bytes.
- **Verification/recompute**: identical to the existing behavior — stream the object back,
  recompute SHA-256, compare.
- **Re-download avoided?** N/A — this mode has never re-downloaded; it hashes
  on the original write/read-back streaming tap. No regression either way.

#### Mode 2 — multipart, SHA-256 offset-manifest composite

- **Per-part computation**: flat `sha256(part_bytes)` — exactly what
  `upload_part` already computes (`s3.rs:803`, `in_memory.rs:190`). No
  change needed to the per-part hash itself, only to what is retained
  (the byte offset, alongside the digest) and what happens with it at
  `complete`.
- **Manifest construction**: at `complete_multipart`, build the canonical
  manifest string described in full in §3 — one entry per part, in ascending
  part-number/offset order, each entry recording that part's **start offset
  within the assembled object** and its `sha256(part_bytes)` digest in hex.
  The manifest is a plain, human-readable, canonically-ordered text
  encoding — not a binary or opaque blob — so it is easy to inspect, log, and
  independently re-implement.
- **Root computation**: `root = sha256(manifest_bytes)`, where
  `manifest_bytes` is the manifest string's UTF-8 encoding. This is a
  **32-byte digest of the manifest text**, not of the object's bytes — a
  deliberate, one-level Merkle-style composite: leaves are per-part SHA-256
  digests (recording *what* each part contains), the manifest is the ordered
  concatenation of leaves plus their offsets (recording *how* the object was
  split), and `root` is a single SHA-256 over that ordered record.
- **Stored fields**: `hash_algorithm = 'SHA-256'`, `hash_mode =
  'multipart-composite-sha256'`, `hash_value = root` (32 bytes, same shape
  and CHECK as mode 1's), **plus `part_count`** (also derivable by parsing
  the manifest, but stored redundantly on `file_versions` so simple
  "does this version's shape look sane" queries — and the manifest-size
  bound in §11 — don't require fetching and parsing the manifest itself),
  **plus the manifest itself**, stored per the decision in §4.
- **Client re-verification is self-contained.** A client that has (a) the
  full object bytes, (b) the returned `root`, and (c) the returned
  `manifest` can independently re-verify with cryptographic precision,
  using nothing but a stock SHA-256 implementation: parse the manifest into
  its ordered `(offset, digest)` entries; split the object at those offsets
  (the final part's length is `object_size - last_offset`, `object_size`
  being the version's already-known `size`); compute `sha256` of each
  resulting slice and confirm it matches the manifest's recorded digest for
  that slice; re-serialize the manifest from the recomputed digests using
  the exact encoding in §3; compute `sha256(reserialized_manifest)` and
  confirm it equals `root`. No knowledge of *how* the split was chosen, no
  access to any multipart-session state, and no trust in the server's
  claimed offsets alone is required — the offsets are cross-checked by the
  fact that they must reproduce the same manifest bytes whose hash is
  `root`.
- **Re-download avoided at complete-time?** **Yes** — this is the entire
  point of this mode. `complete_multipart` for this mode never issues a `GetObject` to compute the hash; it builds
  the manifest and its root from the already-computed, already-durable
  `multipart_upload_parts.part_hash` digests plus the already-known part
  offsets (`compute_plan` produces `(part_number, offset, size)`). Separately, the control-plane `complete` endpoint
  performs one small ~8 KiB ranged read of the assembled object for post-assembly MIME sniffing (content-type
  validation) -- an unrelated concern from hash computation, and far smaller than a full object re-read.

#### Contrast table

| | Mode 1 | Mode 2 |
|---|---|---|
| Trigger | non-multipart | multipart |
| `hash_algorithm` | `SHA-256` | `SHA-256` |
| `hash_mode` | `whole-sha256` | `multipart-composite-sha256` |
| `hash_value` = `sha256(object bytes)`? | Yes | **No** — `sha256(manifest)`, a hash of content *and* split layout |
| Manifest stored? | No | Yes (§4) |
| Re-download at complete? | N/A | No |
| Re-verify from object bytes alone? | Yes, always | Yes, **but only together with the stored manifest** — self-contained once both are in hand, not from object bytes alone |
| Cross-mode identity (same content, both modes → same digest)? | — | **No, by design** — see §11's split-dependent-identity risk |

### 3. Manifest encoding — canonical wire format

The manifest must be a **byte-for-byte reproducible** serialization: any two
independent implementations given the same ordered `(offset, digest)` pairs
must produce the exact same manifest bytes, because `root = sha256(manifest)`
depends on every byte of it. This section is the wire-format specification;
treat it as normative.

**Grammar** (ABNF-ish):

```
manifest    = version "," part *("," part)
version     = "v1"
part        = offset ":" digest
offset      = "0" / (nonzero-digit *digit)      ; decimal, no leading zeros, no sign
digest      = 64(hex-lower)                     ; sha256(part_bytes), lowercase hex, exactly 64 chars
hex-lower   = %x30-39 / %x61-66                  ; '0'-'9', 'a'-'f' — lowercase only, no uppercase
```

**Example** (3 parts, part 0 is 8 MiB, part 1 is 8 MiB, part 2 is the 3 MiB
tail):

```
v1,0:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08,8388608:3f79bb7b435b05321651daefd374cdc681dc06faa65e374e38337b88ca046dea,16777216:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae
```

**Encoding rules, stated exhaustively so two independent implementations
never diverge:**

1. **Version prefix.** The manifest always begins with the literal `v1`
   followed by a comma. This is a format version, not a hash-algorithm or
   part-count field — it exists so a future incompatible change to the
   manifest grammar (e.g. a `v2` that changes the digest algorithm or
   delimiter scheme) can be distinguished from `v1` manifests without
   ambiguity. Parsers MUST reject any manifest whose prefix is not a version
   token they understand.
2. **Ordering.** Parts appear in strictly ascending **byte-offset** order,
   which is identical to strictly ascending part-number order for any valid
   multipart plan (part boundaries are monotonic and non-overlapping by
   construction). The first part's offset is always `0`. There is no
   independent "part number" field in the manifest — the position in the
   ordered list *is* the part number, and the byte offset is what a
   re-verifier actually needs to slice the object.
3. **Offset representation.** Decimal, ASCII digits only, no leading zeros
   (except the literal value `0` itself), no sign, no thousands separators,
   no whitespace. `u64`-range is sufficient for any realistic object size.
4. **Digest representation.** Exactly 64 lowercase hex characters — the
   SHA-256 digest of that part's raw bytes, encoded the same way
   `hash::sha256`'s existing hex-encoding helper would (if one does not
   already exist with these exact properties — lowercase, zero-padded,
   no `0x` prefix — the manifest-building code must produce output
   equivalent to `format!("{:02x}", byte)` per byte, concatenated). Uppercase
   hex, `0x`-prefixed hex, or base64 MUST NOT appear — any of those would be
   an equally valid *encoding* of the same digest bytes but a **different
   manifest string**, hence a **different `root`**, so the encoding must be
   fixed exactly once and never vary by implementation, locale, or library
   default.
5. **Delimiters, and why no escaping is needed.** `,` separates part entries;
   `:` separates a part's offset from its digest. Both are safe as bare
   delimiters with **no escaping discipline required**, because neither
   character can ever occur inside a `offset` or `digest` token as defined by
   the grammar above (offsets are `[0-9]+`, digests are `[0-9a-f]{64}`) — the
   grammar is delimiter-injection-proof by construction, not by escaping.
   This is a deliberate simplicity win over formats that need quoting rules
   (CSV, JSON string fields): fixed-charset tokens plus fixed-position
   delimiters is sufficient here specifically because every field's alphabet
   is known and disjoint from the delimiter characters.
6. **No trailing delimiter, no whitespace anywhere.** The manifest ends
   immediately after the last part's digest — no trailing comma, no trailing
   newline, no surrounding whitespace of any kind. `manifest_bytes` is the
   manifest string's UTF-8 encoding, which (given the grammar above) is
   always pure ASCII, so UTF-8 vs. any other single-byte-clean encoding is
   moot in practice, but UTF-8 is the normative choice for
   `sha256(manifest_bytes)`'s input.
7. **Part count is implicit but also stored redundantly.** The number of
   `part` entries in the manifest always equals the version row's
   `part_count` column (§5); a manifest whose entry count disagrees with the
   stored `part_count` **should** be treated as corrupt/a verification
   failure, not silently trusted from either source alone. **Known limitation:
   not implemented.** `Store::verify_content_hash` takes no
   `part_count` parameter and `migrate_backend` never compares the manifest's
   entry count against the stored column — this cross-check does not
   run anywhere in the code. Do not infer it is enforced from this
   normative description.
8. **Empty / degenerate cases.** A multipart upload always has at least one
   part by construction (S3 and this gear's own multipart flow both reject
   zero-part completion), so the manifest always has at least one `part`
   entry (`v1,0:<digest>` at minimum). There is no "empty manifest" case to
   define. **Amendment (ADR-0006 single-part amendment):** the one-entry
   manifest remains valid *as an encoding*, but `complete` no longer
   produces one — a plan of exactly one part finalizes as `whole-sha256`
   with `hash_value = sha256(object bytes)` (== the single part's digest)
   and no manifest row; only plans of ≥2 parts store a manifest.

**Why a plain-text, human-inspectable format instead of a packed binary
encoding** (e.g. concatenated raw `u64` offsets + 32-byte digest bytes): the
manifest is returned over the API and is meant to be independently
re-implementable by any client with a stock SHA-256 library and a text
parser — no custom binary framing, no endianness question, no length-prefix
convention to get subtly wrong. The size cost (hex digests are 2× the raw
byte count) is accepted deliberately; §11 shows this is still bounded to
well under a megabyte at realistic part counts.

### 4. Manifest storage

**Decision: a dedicated `version_hash_manifest` table, keyed by
`version_id`, not a sidecar object in the backend and not an inline column on
`file_versions`.**

```sql
CREATE TABLE file_storage.version_hash_manifest (
    version_id  uuid  NOT NULL PRIMARY KEY REFERENCES file_storage.file_versions(id) ON DELETE CASCADE,
    manifest    text  NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
```

**Alternatives considered:**

* **Inline column on `file_versions`** (e.g. `file_versions.manifest text
  NULL`) — rejected. `file_versions` is the hot row read on every metadata
  fetch, list, and download-path lookup; a ~800 KB worst-case manifest
  (§11) sitting on that row means every unrelated read of that row (e.g. a
  `SELECT size, mime_type FROM file_versions WHERE id = $1` that never asks
  about the manifest) pays for fetching and potentially TOAST-decompressing
  a large text value it does not need, and it bloats the row's physical
  storage even when the manifest is never subsequently read. Splitting it
  into its own table (a natural 1:1 "detail" table, the same pattern as
  keeping large BLOB/CLOB payloads out of a frequently-scanned parent row)
  avoids this entirely — the manifest is fetched only when a caller actually
  asks to re-verify.
* **Sidecar object in the backend** (`{backend_path}.manifest`, stored as a
  second object next to the content object in S3/local-fs/in-memory) —
  considered, not chosen, but recorded as a viable alternative. It would
  avoid growing the control-plane database at all (manifests could total
  many GB in aggregate at fleet scale, though at the same order of magnitude
  as the objects they describe divided by ~10k, so proportionally small).
  Rejected as the primary choice because:
  * It gives up **transactional consistency** with the version row — the
    manifest and the `file_versions` row would be written via two separate
    systems (backend PUT + DB INSERT) with no shared transaction, reopening
    a partial-write hazard (`complete_multipart` succeeds, manifest object
    write fails or vice versa) that a single DB transaction covering
    `file_versions` + `version_hash_manifest` avoids for free.
  * It complicates **API retrieval** — returning the manifest in a metadata
    response would require an extra backend round-trip (fetch the sidecar
    object) on top of the DB row fetch, rather than a single join/query
    against the control-plane database the metadata endpoint already talks
    to.
  * It complicates **backend migration** (`migrate_backend`) — a backend
    copy would need to additionally discover and copy the sidecar manifest
    object using its own naming convention, rather than the manifest simply
    riding along with the version row that `migrate_backend` already reads.
  * It is a better fit for a future scale point where manifest storage
    volume in Postgres becomes a real operational concern; nothing in this
    design forecloses migrating to it later, since the manifest's *content*
    (§3) is storage-location-independent — only the *retrieval path* would
    change, not the wire format or verification story.
* **Retaining `multipart_upload_parts` rows indefinitely** — considered, and
  **not needed under this design.** Because the manifest is self-contained
  (it already records every part's offset and digest inline), there is no
  need to keep the ephemeral multipart-session's `multipart_upload_parts`
  rows around after `complete` succeeds — the manifest is a complete,
  standalone snapshot of everything `multipart_upload_parts` would otherwise
  need to retain. `multipart_upload_parts` can keep its existing
  session-scoped lifecycle and cleanup policy entirely unchanged.

**Justification summary for the chosen table:** transactional consistency
with `file_versions` (single DB transaction covers both at `complete`/
`finalize` time), trivial API retrieval (one additional indexed lookup by
`version_id`, no backend round-trip), and no new per-backend naming
convention to invent and keep in sync across S3/in-memory/local-fs. The
sidecar-object alternative remains available as a documented fallback if
manifest storage volume in Postgres ever becomes the bottleneck.

### 5. Schema changes

`file_versions` (currently `hash_algorithm text CHECK(= 'SHA-256')`,
`hash_value bytea CHECK(octet_length = 32)`) gains a mode discriminator and a
part count; **`hash_algorithm`'s CHECK is unchanged** — it stays locked to
`'SHA-256'` for both modes, never widened:

```sql
ALTER TABLE file_versions
    ADD COLUMN hash_mode  text NOT NULL DEFAULT 'whole-sha256'
        CHECK (hash_mode IN ('whole-sha256', 'multipart-composite-sha256')),
    ADD COLUMN part_count integer,  -- NOT NULL only for hash_mode = 'multipart-composite-sha256'
    ADD CONSTRAINT file_versions_part_count_presence_check
        CHECK ((hash_mode = 'multipart-composite-sha256') = (part_count IS NOT NULL));
    -- hash_algorithm CHECK (hash_algorithm = 'SHA-256') is NOT touched — both
    -- modes use SHA-256 as the only underlying primitive; there is nothing
    -- to widen.
```

Plus the new manifest table from §4:

```sql
CREATE TABLE file_storage.version_hash_manifest (
    version_id  uuid  NOT NULL PRIMARY KEY REFERENCES file_storage.file_versions(id) ON DELETE CASCADE,
    manifest    text  NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
```

No new column is needed on `multipart_uploads` — there is exactly **one**
multipart mode. Every multipart completion always produces a
`multipart-composite-sha256` version; every non-multipart completion always
produces a `whole-sha256` version. The mode is a function of *which code path
ran*, not of any persisted configuration (§9).

The existing `octet_length(hash_value) = 32` CHECK on `file_versions` is
unchanged — `root` is a SHA-256 digest (32 bytes) exactly like the
whole-object hash, so no widening is needed there either: no algorithm CHECK
widening, no `part_size` column (offsets are self-describing inside the
manifest, so there is no uniform-part-size invariant to record or enforce),
one new small table instead of a wider `file_versions` row.

**`VersionRepo::finalize`** (`repo/version_repo.rs:144-` and its caller
`Store::finalize_version`, `store/versions.rs:133-167`) must gain
`hash_mode: &str, part_count: Option<i32>` parameters (no `hash_algorithm`
parameter needed — it never varies) and write them at finalize time, plus
(for `multipart-composite-sha256`) insert the `version_hash_manifest` row in
the same transaction. `hash_algorithm` is fixed at **pending-insert**
time (`store/mod.rs::pending_version`) and `hash_mode` would need the same
"decided at finalize, not at pending-insert" treatment, for the same reason
noted in §1: a pending row is created before it is known whether the upload
will complete as single-part or multipart.

**`multipart_upload_parts.part_hash`**: unchanged in shape — continues to
store the flat per-part SHA-256 digest it already stores. Recommend
(independent of this project, a pre-existing latent gap) adding a
`NOT NULL CHECK (octet_length(part_hash) = 32)` while touching this table.
No retention-policy change is needed for this table (§4) — it keeps its
existing session-scoped lifecycle.

### 6. Verification / recompute paths

| Path | Existing | Mode 1 (`whole-sha256`) | Mode 2 (`multipart-composite-sha256`) |
|---|---|---|---|
| Single-part `finalize_upload[_by_token]` (`write.rs`) | re-read whole object, `hash::sha256`, compare | unchanged | N/A (multipart only) |
| Multipart `complete_multipart_upload` (`multipart_service.rs`) | `backend.complete_multipart` re-reads + flat SHA-256 | N/A | build manifest from already-collected `(offset, part_hash)` pairs, `root = sha256(manifest)` — **no re-read** |
| Client-side re-verification | N/A (no multipart mode existed with an independent client check) | re-read/re-fetch the object, `sha256`, compare to `hash_value` — always possible from object bytes alone | **retain the composite root (wire field `content_hash` on the `POST .../complete` response) and `manifest`** (they are not re-fetchable later — `VersionDto`/`GET /files/{id}/versions` has no `manifest` field, a real limitation of the current API surface), split the object at the manifest's recorded offsets, `sha256` each part, rebuild the manifest string per §3, `sha256(manifest) == root` — self-contained given object bytes + the retained manifest; not possible from object bytes alone |
| `migrate_backend` (`backend.rs:35-170`) | `Store::verify_content_hash` = hard-coded `hash::sha256(blob)` | unchanged: re-read + whole-object SHA-256 rehash, compare to `hash_value` | fetch the `version_hash_manifest` row alongside the version; re-read the (already necessarily re-read, since this is a backend copy) object bytes; split at the manifest's offsets, `sha256` each part, rebuild the manifest, compare `sha256(manifest)` to `hash_value` — **fully self-contained from object bytes + the stored manifest row, no dependency on `multipart_upload_parts` surviving** |
| Any future generic "re-verify a version's integrity" tool | implicit, whole-object | dispatch by `hash_mode`, whole-object rehash | dispatch by `hash_mode`; fetch the manifest row, re-derive per the migrate_backend path above |

`Store::verify_content_hash` (`store/mod.rs:154`) is mode-aware:
`fn verify_content_hash(blob, hash_mode, hash_value, manifest: Option<&str>)
-> Result<(), DomainError>`. For `whole-sha256`, `manifest` is always `None`
and the function's behavior is unchanged. For `multipart-composite-sha256`,
`manifest` is required (`None` here is a caller bug, not a runtime
ambiguity — every `multipart-composite-sha256` version has exactly one
`version_hash_manifest` row by construction, §5's `1:1` FK relationship) and
the function performs the split-rehash-rebuild-compare sequence above.

**Every verification path for mode 2 is self-contained from (object bytes +
the small, cheaply-retained manifest row), for the lifetime of the
version.** There is no "must retain `multipart_upload_parts` forever"
question left open — the manifest *is* the durable, complete record that
`multipart_upload_parts` would otherwise have needed to become.

### 7. Trait changes

```rust
/// One of the two hash modes; carried end-to-end from the multipart
/// plan through to the stored version row. `hash_algorithm` is not part of
/// this enum — it is always SHA-256 for both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashMode {
    WholeSha256,
    MultipartCompositeSha256,
}

/// One manifest entry: a part's start offset within the assembled object,
/// plus its SHA-256 digest. `Manifest::to_wire_string()` / `from_wire_string()`
/// implement the exact grammar in §3 — the single canonical place this
/// encoding is produced or parsed.
pub struct ManifestEntry {
    pub offset: u64,
    pub digest: [u8; 32],
}
pub struct Manifest(Vec<ManifestEntry>);

#[async_trait]
pub trait StorageBackend: Send + Sync {
    // ... id, capabilities, put, get, delete, exists, list_paths,
    // put_stream, get_stream, initiate_multipart, abort_multipart ...

    /// Upload one part. `part_offset` is a required input so the manifest
    /// can record it at `complete` time without re-deriving it from a plan
    /// the backend itself may not retain.
    async fn upload_part(
        &self,
        path: &str,
        upload_handle: &str,
        part_number: u32,
        part_offset: u64,                  // needed for the manifest, not for hashing itself
        data: Bytes,
    ) -> Result<(String, [u8; 32]), DomainError>; // (backend_etag, sha256(part_bytes))

    /// Complete a multipart upload. The ordered `(offset, part_hash,
    /// backend_etag)` triples are an input, not discarded. The backend performs
    /// its own native completion (S3 `CompleteMultipartUpload`, in-memory
    /// assembly, etc.) and builds the manifest and its root from `parts`
    /// rather than re-reading the assembled object.
    async fn complete_multipart(
        &self,
        path: &str,
        upload_handle: &str,
        parts: &[(u32, u64, [u8; 32], String)], // (part_number, offset, part_hash, backend_etag)
    ) -> Result<(Manifest, [u8; 32]), DomainError>, // (manifest, root)
}
```

Key points:
- `complete_multipart`'s contract is to build the manifest and root from the
  hashes and offsets already collected during upload, not to re-read and hash
  the assembled object — this applies to the *only* multipart mode there is,
  so every backend's multipart-capable implementation needs exactly one
  `complete_multipart` arm.
- `upload_part` takes the part's byte offset — already known by the caller
  (`compute_plan` produces it) — as part of the trait call.
- `MultipartService::complete_multipart_upload` (`multipart_service.rs`)
  threads `(part_number, offset, part_hash, backend_etag)` for every part
  into `complete_multipart`, and persists the returned `Manifest` into
  `version_hash_manifest` in the same transaction as `finalize_version`.
- `finalize_version`/`VersionRepo::finalize` take `(hash_mode, part_count)`
  parameters (§5); no `hash_algorithm` parameter is needed since it never
  varies.
- The `Manifest` type's `to_wire_string()` is the **single, shared**
  implementation of §3's grammar — every backend calls the same function
  rather than each backend hand-rolling string formatting, which is exactly
  the kind of place a subtle divergence (uppercase vs. lowercase hex,
  trailing comma) would silently produce a different `root` per backend.

### 8. Cross-backend mapping

| Backend | Mode 1 (`whole-sha256`) | Mode 2 (`multipart-composite-sha256`) |
|---|---|---|
| **S3** (`s3.rs`) | `put_stream`/`put` | `upload_part` computes a flat per-part SHA-256 and threads `part_offset` through; `complete_multipart` calls `CompleteMultipartUpload` (S3 still needs the ETags to assemble) **then builds the manifest and computes `root` from the already-collected `(offset, part_hash)` pairs**, without calling `get_and_hash_streaming`. **Every large multipart upload thereby avoids the mandatory re-`GetObject`** a full re-read would otherwise cost — no redundant read of a potentially multi-GB object, no doubled egress/bandwidth. |
| **In-memory** (`in_memory.rs`) | as above | `upload_part` as above; `complete_multipart` builds the manifest/root instead of `hash::sha256(&assembled)` (it still assembles bytes into the blob store for `get`, but computing the **hash** does not require touching those bytes) |
| **local-fs** (`local_fs.rs`) | unchanged (single-object writes only) | **N/A — still no multipart support.** `initiate_multipart`/`upload_part`/`complete_multipart`/`abort_multipart` remain the trait's default `Err(multipart_not_supported)`. If local-fs multipart is ever added, it needs no special accommodation for this mode beyond any other backend — offsets and per-part digests are backend-agnostic inputs to the same shared `Manifest` builder. |

### 9. Mode selection

**There is no algorithm or mode *choice* to make in this design — mode is
determined entirely by upload path, not by configuration or client
preference.**

- **Non-multipart upload → always `whole-sha256`.** No configuration knob,
  no per-request hint, no default to set.
- **Multipart upload → always `multipart-composite-sha256`.** Same — no
  choice, because there is only one multipart mode. `multipart_uploads`
  needs no `hash_mode` column (§5) because the value is a constant, not a
  per-session decision.
- **This drops ADR-0002's client-preference / selection-rules /
  discovery-endpoint vision entirely from scope**, not merely defers it —
  there is nothing left to prefer or discover once the algorithm is fixed
  and the multipart strategy is fixed. A future request that reopens
  algorithm choice (e.g. to add a second algorithm for a genuinely new
  reason) would need to reintroduce this machinery from scratch; nothing in
  this design leaves a partial version of it lying around to build on.
- **The stored `(hash_mode, hash_value, part_count)` on the version row,
  plus the `version_hash_manifest` row where applicable, is always the
  ground truth for "how do I verify this version."** This invariant does
  not depend on current gear config, since nothing about mode selection is
  configurable in the first place under this design.

### 10. FIPS handling

**Trivially clean — no non-approved algorithm anywhere.** SHA-256 is the
*only* primitive in this design, for both modes. There is nothing to gate
behind a Cargo feature, nothing to reject at config-load time under
`--features fips`, and no IG-2.4.A carve-out to invoke to justify a
non-approved algorithm being present at all.

* **Mode 1 (`whole-sha256`)** is exactly the existing whole-object SHA-256 hash —
  the finalize-time `expected_hash` comparison path — and carries an
  **adversarial-integrity claim**, unchanged. It uses SHA-256, a
  FIPS-approved algorithm, in the algorithm's ordinary defined sense (the
  SHA-256 of the object's bytes).
* **Mode 2 (`multipart-composite-sha256`)** is a **non-security-relevant
  integrity/corruption/correct-split-and-upload check** (per FIPS 140-3
  Implementation Guidance IG 2.4.A: a hash used for identity, deduplication,
  or accidental-corruption/correct-assembly detection — not an
  adversarial-integrity or signature claim — need not itself be treated as a
  security-relevant primitive). Concretely: `root` is a **bespoke SHA-256
  construction** — a one-level Merkle-style combination over per-part
  SHA-256 digests plus their offsets — not itself a standards-defined "the
  SHA-256 hash of X" the way mode 1's digest is. It uses **no non-approved
  primitive** (every hash operation inside it, per-part and the final
  manifest hash, is a plain SHA-256 call), so there is no dependency-graph
  exclusion, no feature flag, and no CMVP-discretion caveat to carry. It is
  simply scoped as: not a FIPS-validated adversarial-security function by
  construction (the composite shape is not a recognized FIPS-approved
  algorithm output), but built entirely from FIPS-approved primitives and
  used only for identity/corruption/correct-assembly checking.
* **The adversarial path remains exactly where it was**: a client-supplied
  `expected_hash` is verified server-side against the sidecar's
  on-the-fly-computed value for mode 1; nothing about mode 2 is used as an
  adversarial-tamper check in this design.
* **No Cargo feature, no `deny-fips.toml` interaction, no dependency-graph
  story is needed at all** — this section is intentionally short because
  there is nothing further to resolve.

### 11. Risks & open decisions

Flagged honestly rather than glossed over. Two real, accepted trade-offs
remain:

1. **Manifest storage size is bounded but not tiny.** At the worst realistic
   case — 10,000 parts (`MAX_PART_COUNT`, matching the practical ceiling most
   S3-style backends impose on multipart part count) at ~80 bytes per
   manifest entry (`{offset}:{64-hex-digest}` plus its delimiter, offsets
   realistically ≤13 decimal digits even at multi-terabyte object sizes) — a
   single manifest is **~800 KB**. This is why §4 puts the manifest in its
   own table rather than inline on `file_versions`. The multipart planner
   (`compute_plan`) enforces the minimum part size (`DEFAULT_MIN_PART_SIZE =
   5 MiB`, unconstrained by any power-of-two requirement) as a **floor**, and
   `MAX_PART_COUNT` (10,000) as a **ceiling independent of any one backend's
   native limit**: a `declared_size` that would need more parts than the
   ceiling at the current part size first widens the part size (up to
   `MAX_PART_SIZE`, 5 GiB) to bring the plan back under the ceiling, and only
   rejects the upload if `declared_size` still cannot fit within
   `MAX_PART_COUNT` parts even at `MAX_PART_SIZE` — a backend without S3's
   own 10,000-part cap (e.g. in-memory) is never able to silently produce an
   unbounded number of parts and hence an unbounded manifest.
2. **Split-dependent identity is a real, accepted trade-off, not an
   oversight.** `multipart-composite-sha256`'s `root` is a hash of "content
   *and* split layout," not `sha256(whole bytes)`. Two consequences that
   must be documented clearly and are **accepted, not open**:
   * **The same file content uploaded once as a single part and once as a
     multipart upload produces two different `hash_value`s** (one
     `whole-sha256`, one `multipart-composite-sha256`), even though the
     underlying bytes are byte-for-byte identical. There is no cross-method
     dedup/identity — a deduplication feature built on `hash_value` alone
     would need to additionally re-hash whole-object SHA-256 out-of-band to
     compare a `multipart-composite-sha256` version against a
     `whole-sha256` one, or accept that cross-mode dedup simply does not
     work.
   * **Two multipart uploads of the same content with different part-size
     choices also produce two different `hash_value`s**, because the offsets
     recorded in the manifest differ. This is the direct, intentional cost of
     choosing an offset-manifest design at all, and the one property
     genuinely given up in that trade. Any future feature that wants
     split-independent content addressing across multipart uploads of the
     same content would need a different mechanism (e.g. an additional,
     separately-computed whole-object hash alongside the manifest composite
     — not proposed here, out of scope, but the honest answer to "how would
     you get that property later").
3. **Backward compatibility of existing SHA-256 versions.** Every
   pre-existing row has (post-migration) `hash_mode =
   'whole-sha256'`, which is correct and requires no data
   migration/backfill computation beyond the new column's default value —
   no re-hashing of existing content, no `version_hash_manifest` row needed
   for any pre-existing row.
4. **Scope discipline, restated.** ADR-0002's client-preference/
   selection-rules/discovery-endpoint machinery remains explicitly out of
   scope (§9) — this design does not merely defer it, it removes the reason
   for it to exist at all (there is no algorithm choice left to
   negotiate). A future request that reopens algorithm choice would be a
   distinct project, not an extension of this one.
