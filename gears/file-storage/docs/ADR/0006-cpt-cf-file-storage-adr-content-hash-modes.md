---
status: accepted
date: 2026-07-08
---

# ADR-0006: Content-Hash Modes — Whole-Object SHA-256 & Multipart Offset-Manifest Composite (SHA-256)

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Mode selection model](#mode-selection-model)
  - [The on-the-fly / no-re-read principle and its trust model](#the-on-the-fly--no-re-read-principle-and-its-trust-model)
  - [Manifest definition and canonical encoding](#manifest-definition-and-canonical-encoding)
  - [Client and migrate_backend re-verification](#client-and-migrate_backend-re-verification)
  - [Manifest storage](#manifest-storage)
  - [FIPS handling](#fips-handling)
  - [Schema and trait impact](#schema-and-trait-impact)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Exactly 2 modes, SHA-256-only, on-the-fly computation (chosen)](#exactly-2-modes-sha-256-only-on-the-fly-computation-chosen)
  - [3-mode design with BLAKE3-tree (prior draft, superseded before implementation)](#3-mode-design-with-blake3-tree-prior-draft-superseded-before-implementation)
  - [ADR-0002's full `hash_policy`/selection-rules surface](#adr-0002s-full-hash_policyselection-rules-surface)
  - [Keep the existing SHA-256-only, re-read at finalize](#keep-the-existing-sha-256-only-re-read-at-finalize)
- [More Information](#more-information)
  - [Open decisions and risks](#open-decisions-and-risks)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-file-storage-adr-content-hash-modes`

## Context and Problem Statement

ADR-0002 committed P1 to SHA-256-only and sketched a P2 vision (`hash_policy` config, per-request client preference,
capability discovery, per-tenant `selection_rules`) that was never implemented. Independently, the P2 remediation
plan's item 0.1 ("Finalize trusts client-supplied size/hash; never verifies the blob") closed a real vulnerability by
having the control plane re-read the stored blob and recompute its hash before trusting a finalize call's claimed
`size`/`hash_value`. That fix is correct but expensive: `finalize_upload`/`finalize_upload_by_token`
(`src/domain/service/write.rs`) stream the object back from the backend and rehash it, and
`S3Backend::complete_multipart` (`src/infra/backend/s3.rs`) issues a full `GetObject` re-read after
`CompleteMultipartUpload` for the same reason. For a multi-GB multipart object this is a full extra read pass — doubled
egress/bandwidth and a real cost at fleet scale — paid on every completed upload.

The maintainer has since set a new, narrower P2+ plan that supersedes ADR-0002's open-ended P2 vision (see ADR-0002's
Superseded Note) and reconciles the 0.1 fix with an on-the-fly trust model that avoids the re-read.
[`../features/content-hash-modes.md`](../features/content-hash-modes.md) is the detailed design document behind this ADR. An earlier pass of both that document and this ADR
explored a 3-mode design built around `blake3::hazmat`'s canonical subtree-combination API (non-multipart SHA-256 or
BLAKE3, multipart BLAKE3-tree, multipart composite-SHA-256). The maintainer has since simplified the decision further:
**BLAKE3 is dropped entirely.** There are exactly **two** content-hash modes, both built on SHA-256 alone, distinguished
only by upload path (non-multipart vs. multipart) — not by user or operator choice.

This ADR records the resulting decision: exactly two content-hash modes, both SHA-256, computed on-the-fly during
upload with no re-read of the stored object, with no non-approved algorithm anywhere in the design.

## Decision Drivers

* **Close the 0.1 vulnerability without its current cost** — a client must not be able to forge `size`/`hash_value` at
  finalize time, but the fix that closed this (re-read and recompute) doubles I/O on every upload; an on-the-fly
  computation that the sidecar performs as bytes stream to the backend closes the same vulnerability for free
* **Multipart finalization cost** — the load-bearing argument for eliminating the finalize-time re-read is that P2
  multipart upload is live enough now to cash in that saving, but only if the re-read is actually eliminated, not
  just theoretically eliminable
* **Independent client verifiability** — a hash mode a client can only trust because the server says so is a weaker
  integrity primitive than one the client can independently re-derive from the object bytes plus a small amount of
  server-supplied metadata using nothing but a stock SHA-256 implementation
* **Scope discipline** — ADR-0002's full `hash_policy`/`selection_rules`/capability-discovery surface is a much larger
  project than "hash this upload correctly"; nothing in the current code exercises per-tenant routing rules, and once
  there is no algorithm choice left to make, that entire surface has no remaining reason to exist, not merely a reason
  to defer building it
* **FIPS posture** — SHA-256 must remain the only content-hash primitive; introducing any second algorithm (as the
  prior BLAKE3-based draft did) reopens a FIPS-gating, feature-flag, and dependency-graph story this decision now
  avoids entirely by using SHA-256 for both modes
* **Arbitrary part sizing** — the prior BLAKE3-tree draft's canonical-combination property depended on a hard
  part-size constraint (every non-final part exactly the same power-of-two-multiple-of-1024-byte size) the current
  multipart planner does not satisfy; a design that works for arbitrary, even varying, part sizes removes an entire
  class of planner constraints and future-drift risk
* **Self-contained re-verifiability without unbounded retention** — the prior draft's composite-SHA-256 mode could
  only be re-verified by retaining the ordered per-part digests in `multipart_upload_parts` indefinitely, an
  unresolved, unbounded storage-growth commitment; a design that folds the retained-digest requirement into a small,
  durable, per-version record resolves this cleanly
* **Trust model** — the sidecar (`cpt-cf-file-storage-adr-sidecar-data-plane`) is the only component that ever touches
  raw bytes and holds backend credentials; it is a trusted, token-authenticated internal component, unlike the client.
  The control plane can trust a hash the sidecar computed from bytes it actually streamed, without needing to
  independently re-derive it from storage

## Considered Options

* Exactly 2 modes (non-multipart whole-object SHA-256; multipart SHA-256 offset-manifest composite), computed
  on-the-fly during upload, no re-read — **chosen**
* 3-mode design with BLAKE3-tree for multipart (this ADR's own prior draft, before implementation) — superseded
* Build ADR-0002's full `hash_policy`/`selection_rules`/capability-discovery surface now
* Keep the existing SHA-256-only, re-read-at-finalize shape indefinitely (do nothing beyond the 0.1 fix already shipped)

## Decision Outcome

Chosen option: **exactly two content-hash modes, both SHA-256, each computed on-the-fly during upload, with no
re-*hashing* re-read of the stored object at multipart `complete` time.** (Single-part `finalize` retains its
existing whole-object read-back for defense-in-depth size/hash/MIME re-verification — see point 3 below; only the
multipart-composite mode's assembly step avoids a full re-read.)

1. **Non-multipart upload → plain `sha256(whole object bytes)`.** The canonical whole-object hash — unchanged in
   shape. Computed by the sidecar's `put_stream` as bytes transit it. It is **not** represented as a
   1-part manifest; there is no manifest at all for this mode.
2. **Multipart upload → SHA-256 offset-manifest composite.** Per part, `sha256(part_bytes)` is computed on-the-fly
   during that part's upload (already done). At `complete_multipart`, a canonical **manifest** string is built
   from the already-collected per-part digests and their byte offsets, in ascending order:
   `"v1,{offset_0}:{hex(sha256(part_0))},{offset_1}:{hex(sha256(part_1))},…"`. The stored digest is
   **`root = sha256(manifest)`** — a compact 32-byte value. Both `root` (as the version's `hash_value`) and the
   `manifest` string are stored and returned by the API, so a client that has the file can independently re-verify
   with cryptographic precision: split the object at the recorded offsets, `sha256` each part, rebuild the manifest,
   check `sha256(manifest) == root`.
3. **The multipart composite hash is never computed by re-downloading/re-reading the assembled stored object.** It is
   always computed on-the-fly, from the per-part digests already collected as bytes flow to the backend, inside the
   sidecar's streaming `upload_part`. This removes the S3 `complete_multipart` re-`GetObject` that a flat-rehash
   design would otherwise need. **This does not extend to single-part uploads**: `write.rs`'s
   `read_back_and_hash_streaming` is still called on every single-part `finalize` (both `finalize_upload` and
   `finalize_upload_by_token`) as a defense-in-depth re-derivation of size/hash/MIME from the real backend object —
   that read-back was never proposed for elimination by this ADR and remains unchanged. `complete_multipart` does
   still issue one bounded (~8 KiB) ranged `GetObject`/`get_range` against the assembled object, but only for MIME
   magic-byte sniffing (P2 remediation item 1.10), not to (re)compute the hash. See
   [below](#the-on-the-fly--no-re-read-principle-and-its-trust-model) for why this still closes the 0.1
   vulnerability.
4. **Mode selection is not a user- or operator-facing choice.** There is no per-request algorithm override and no
   gear-level default-config knob to set — the mode is a pure function of which upload path executed. A
   non-multipart completion always produces `whole-sha256`; a multipart completion always produces
   `multipart-composite-sha256`. `hash_algorithm` never varies — it is `SHA-256` for both modes, always.

### Mode selection model

There is **no algorithm or mode choice to configure, override, or discover.** This is a deliberate simplification
over both the prior 3-mode BLAKE3 draft and ADR-0002's original P2 vision:

* No configuration value sets a default algorithm or multipart strategy, because there is exactly one algorithm
  (SHA-256) and exactly one strategy per upload path.
* No per-request hint on upload/multipart-initiate is accepted for hash mode — the server does not need to resolve a
  preference against an allow-list, because there is nothing to prefer between.
* Every version's row records **which mode actually produced its stored hash** — `hash_mode` (`whole-sha256` or
  `multipart-composite-sha256`), set at finalize time from which code path executed, not from any config value. This
  one invariant survives from the earlier drafts even though the reason for it is narrower now: it remains the sole
  ground truth for "how do I verify this version," independent of anything that might change later.
* `multipart_uploads` needs no `hash_mode` column — unlike the earlier 3-mode draft (which needed to record a real
  per-session choice between BLAKE3-tree and composite-SHA-256 before any bytes arrived), the multipart mode here is
  a constant, not a decision.

#### Amendment (2026-07): the one-part plan degenerates to `whole-sha256`

The original decision made "uploaded via multipart" imply the composite mode unconditionally. This amendment narrows
that: **a multipart plan consisting of exactly one part finalizes as `whole-sha256`**, not
`multipart-composite-sha256`. Rationale: for a one-part plan the single part spans the entire object, so the part's
streaming SHA-256 digest **is** `sha256(whole object bytes)` — already computed on the fly, no re-read needed — and
wrapping it in a one-entry manifest (`root = sha256("v1,0:<h>")`) would only make the same bytes hash differently
depending on whether they arrived as a single-shot PUT or a one-part multipart session. Consequences:

* `complete_multipart_upload` stores `hash_mode = 'whole-sha256'`, `hash_value = sha256(bytes)` (the part digest),
  `part_count = NULL`, and persists **no** `version_hash_manifest` row; the `complete` response's `manifest` field is
  omitted (it became `Option` on the wire).
* The mode-selection invariant above is unchanged: `hash_mode` still records which mode actually produced the stored
  hash, set at finalize from the executed path — the "path" now includes the plan's part count.
* §3's "minimum-degenerate" single-entry manifest (`v1,0:<digest>`) remains a **valid encoding at the type level**
  (the grammar has no special case); it is simply never persisted or returned by `complete` anymore.
* **Compatibility**: versions finalized as one-part composites before this amendment keep their stored
  `multipart-composite-sha256` mode and manifest row, and verify by that mode — verification always follows the
  version row's own `hash_mode`, never re-derives it.
* The split-dependent-identity trade-off (risk 2 below) is thereby removed for the one-part case only: a one-part
  multipart upload of some content now hashes identically to a single-shot upload of the same content. Plans of two
  or more parts keep the composite semantics unchanged.

### The on-the-fly / no-re-read principle and its trust model

Every mode is computed **as bytes flow to the backend during upload**, never by reading the object back afterward.
Concretely: the sidecar's streaming write path (`put_stream` for non-multipart, `upload_part`/`complete_multipart` for
multipart) is the sole hash-computation site; `finalize_upload[_by_token]`'s current read-back-and-rehash step and
`S3Backend::complete_multipart`'s post-`CompleteMultipartUpload` `GetObject` re-read are both removed.

**Reconciliation with item 0.1.** 0.1's fix works by having the control plane independently re-derive `size`/
`hash_value` from the stored bytes, so a client cannot forge them by lying in its finalize call. Removing the re-read
does not reopen this: the **trust boundary shifts from "never trust anything not independently re-derived from
storage" to "trust the sidecar's own on-the-fly-computed hash, never the client's claimed hash."** This is a
legitimate narrowing, not a regression, because:

* The sidecar is the **internal, token-authenticated data-plane component** (`cpt-cf-file-storage-adr-sidecar-data-plane`)
  — it is not the untrusted client. It is the only component that ever holds backend credentials and touches raw
  bytes; the control plane already places load-bearing trust in it for path/credential handling.
* The hash the sidecar reports is derived from the **actual bytes it received and wrote**, computed incrementally as
  they transit it — a client cannot influence this value by claiming a different size or hash in its own request; the
  sidecar's streaming hasher only ever sees what was actually written to the backend.
* This still closes the original vulnerability's exact shape ("upload nothing, finalize with a forged size/hash," or
  "upload real bytes but finalize with a mismatched claim") because the client-supplied claim is no longer part of the
  trust chain at all — the stored `(hash_mode, hash_value)` comes from the sidecar's own computation, and any
  client-supplied `expected_hash` is compared **against** that on-the-fly value rather than trusted in its own right
  (as the existing finalize-time hash-mismatch rejection already does).
* What is given up, relative to a full re-read, is independent verification of "did the bytes that ended up in the
  backend match what the sidecar reported" from a source other than the sidecar itself. That residual trust is placed
  deliberately in the sidecar as the system's designated trust boundary, not left with the client.

### Manifest definition and canonical encoding

The multipart mode's `root` is `sha256(manifest)`, where `manifest` is a **canonical, byte-for-byte reproducible
text encoding** — normative grammar (full detail in `content-hash-modes.md` §3, summarized here):

```
manifest = "v1" *("," offset ":" digest)
```

where entries appear in strictly ascending byte-offset order (offset `0` first), `offset` is a decimal integer with
no leading zeros, and `digest` is the part's `sha256(part_bytes)` as exactly 64 lowercase hex characters. No escaping
is needed: the offset and digest alphabets (`[0-9]+` and `[0-9a-f]{64}`) can never contain the `,`/`:` delimiters, so
the grammar is delimiter-injection-proof by construction. There is no trailing delimiter and no whitespace. Two
independent implementations given the same ordered `(offset, digest)` pairs always produce byte-identical manifest
text, which is required because `root` is a hash of every byte of it.

The manifest fully records **how the object was split**, not just what each part contains — this is what makes
client-side re-verification possible without any out-of-band knowledge of the upload's part-size policy.

### Client and migrate_backend re-verification

Because the manifest records offsets and per-part digests, and both `root` and `manifest` are returned by the
`POST .../multipart/{upload_id}/complete` response, **every verifier — a client, or the control plane's own
`migrate_backend`** — can independently re-derive everything from the object bytes plus the manifest, with no
dependency on any other retained state. **Tracked API-exposure gap**: `manifest` is returned only in that one-shot
`complete` response — `VersionDto` (`GET /files/{id}/versions`) has no `manifest` field, so a client that wants to
re-verify **later** (not at complete time) must have retained the manifest text itself from the original `complete`
response; there is no way to re-fetch it from the control plane's REST surface today. `migrate_backend` does not
have this limitation — it reads `version_hash_manifest` directly, server-side.

1. Split the object at the manifest's recorded offsets (the final part's length follows from the version's known
   `size`).
2. Compute `sha256` of each resulting slice and confirm it matches the manifest's recorded digest for that slice.
3. Re-serialize the manifest from the recomputed digests using the exact grammar above.
4. Compute `sha256` of the re-serialized manifest and confirm it equals `root`.

**This makes the design self-contained**: `migrate_backend`'s "read whole blob, recompute, compare" shape (hard-coded
to whole-object SHA-256) becomes mode-aware — for `whole-sha256` it is unchanged; for
`multipart-composite-sha256` it fetches the manifest row alongside the version and performs the sequence above. **This
resolves the P2+ plan's previously-open decision on whether `multipart_upload_parts` rows must be retained
indefinitely for re-verification** (the prior 3-mode draft's single biggest open item): they do not need to be,
because the manifest — not the ephemeral per-session parts table — is the durable, complete, self-contained record of
everything needed to re-verify a multipart-composite-mode version, for the version's entire lifetime.

### Manifest storage

**Decision: a dedicated `version_hash_manifest(version_id, manifest text)` table**, one row per
`multipart-composite-sha256` version, `version_id` as primary key and FK into `file_versions`. Chosen over two
alternatives:

* **Inline column on `file_versions`** — rejected. Worst case ~10,000 parts × ~80 bytes/entry ≈ ~800 KB per manifest;
  putting that on the hot `file_versions` row (read on every metadata fetch, list, and download-path lookup) would
  make every unrelated read of that row pay for a large value it usually does not need.
* **Sidecar object in the backend** (`{backend_path}.manifest`) — a viable alternative, recorded but not chosen. It
  keeps the manifest out of the control-plane database entirely, at the cost of transactional consistency (the
  manifest write and the `file_versions` write would be two separate systems with no shared transaction) and simple
  API retrieval (a metadata response would need an extra backend round-trip rather than a single DB join). The
  separate-table choice gives transactional consistency (a single DB transaction covers both `file_versions` and
  `version_hash_manifest` at finalize) and trivial API retrieval; the sidecar-object approach remains available as a
  documented fallback if manifest storage volume in Postgres ever becomes an operational bottleneck at fleet scale.

A bounded **minimum part size** (already `DEFAULT_MIN_PART_SIZE = 5 MiB`) must continue to be enforced, and the
resulting maximum part count enforced as a ceiling (recommend `MAX_PART_COUNT`, e.g. 10,000, matching the realistic
ceiling most S3-style backends already impose) independent of any one backend's native cap, so a backend without such
a native limit cannot silently produce an unbounded manifest.

### FIPS handling

**Trivially clean.** Everything is SHA-256 — a FIPS-approved algorithm — for both modes. There is **no non-approved
algorithm anywhere in this design**, so none of the machinery the earlier BLAKE3-based draft required is needed:

* **No Cargo feature gating.** There is no second hash crate to add, no `blake3-hash` feature, nothing to make
  mutually exclusive with `--features fips`.
* **No IG-2.4.A carve-out is needed to justify a non-approved algorithm** — there is no non-approved algorithm to
  justify. The content hash remains what it always was: a non-security-relevant integrity/corruption/
  correct-split-and-upload check, built entirely from SHA-256 (FIPS-approved) in both modes.
* **Mode 1** (`whole-sha256`) plus the finalize-time `expected_hash` verification path remain the design's
  adversarial-integrity-relevant surface, unchanged.
* **Mode 2** (`multipart-composite-sha256`) is a **bespoke SHA-256 construction** — a one-level Merkle-style
  combination over per-part SHA-256 digests and their offsets — not itself a standards-defined "the SHA-256 hash of
  X" the way mode 1's digest is, and not a FIPS-scoped adversarial-security function by construction. It uses no
  non-approved primitive: every hash operation inside it (per-part digests, the final manifest hash) is a plain
  SHA-256 call. It is scoped to identity/corruption/correct-assembly detection only, the same non-adversarial
  treatment ADR-0002 already gives XXH3.
* **No dependency-graph exclusion, no `deny-fips.toml` interaction, and no CMVP-discretion caveat are needed** — the
  prior draft's entire FIPS section (Cargo feature mutual exclusion with `fips`, config-load-time rejection,
  `cargo tree --features fips` verification, a documented BLAKE3 module analogous to `hash.rs`'s DE0708 allow-list
  entry) is removed wholesale, not merely simplified, because none of it has anything left to gate.

### Schema and trait impact

Full detail — exact migration DDL, `StorageBackend` trait signature changes, `HashMode`/`Manifest` types, and the
per-backend (`S3Backend`/`InMemoryBackend`/`LocalFsBackend`) mapping — is in `content-hash-modes.md` §5, §7, §8 and is
not duplicated here. Summary: `file_versions` gains `hash_mode` (`'whole-sha256'` | `'multipart-composite-sha256'`)
and `part_count` (multipart mode only) columns; **`hash_algorithm`'s existing `CHECK (hash_algorithm = 'SHA-256')` is
left untouched** — it is never widened, since both modes use SHA-256 as their only underlying primitive. A new
`version_hash_manifest` table stores the manifest text, 1:1 with multipart-mode version rows. `hash_mode`/`part_count`
move from being unset at pending-insert time to being set at finalize time (the existing `VersionRepo::finalize` never
touches `hash_algorithm`, a gap independent of this decision); `StorageBackend::upload_part` gains a `part_offset`
parameter and `complete_multipart`'s contract flips from "re-read and hash the assembled object" to "build the
manifest and root from the hashes and offsets you were given." **Gap, not enforced anywhere:** `part_count` and the
`version_hash_manifest` row's own entry count are both derived from the same `parts` collection at finalize time, so
they agree in practice, but nothing — no DB constraint, no runtime assertion — cross-checks that `part_count` actually
equals the manifest's entry count after the fact; a future bug in a code path that sets one without the other would
not be caught automatically.

### Consequences

* **Removes the full-object re-hash read pass from multipart completion.** `S3Backend::complete_multipart`'s
  post-completion `GetObject`-and-rehash is eliminated — a genuine bandwidth/cost win at fleet scale for multipart
  uploads specifically. **Single-part `finalize_upload[_by_token]`'s read-back-and-rehash is retained** (defense in
  depth against a forged size/hash claim) and was never in scope for elimination; `complete_multipart` also still
  issues one small bounded ranged read for MIME sniffing (see point 3 of the Decision Outcome above).
* **The stored `(hash_mode, hash_value)`, plus the `version_hash_manifest` row where applicable, is the sole ground
  truth for verification going forward** — not something re-derived from gear config, and not dependent on any
  configuration existing in the first place, since there is none to configure.
* **`migrate_backend`'s integrity story is uniform across both modes**, each fully self-contained: mode 1 verifies via
  a whole-object re-read-and-rehash (unchanged); mode 2 verifies via a re-read plus the split-rehash-
  rebuild-compare sequence against the stored manifest — no dependency on `multipart_upload_parts` surviving past the
  multipart session's own lifecycle.
* **No new dependency, no new FIPS-gating infrastructure.** Unlike the prior BLAKE3-based draft, this decision adds no
  Cargo dependency and needs no feature-flag story — the only new "surface" is the manifest encoding and its storage
  table, both plain SHA-256-based.
* **A real, accepted trade-off: split-dependent identity.** The multipart mode's `root` is a hash of "content and
  split layout," not `sha256(whole bytes)` — it is **not** comparable to a whole-object SHA-256 of the same content,
  and two multipart uploads of the same content with different part-size choices produce different `hash_value`s.
  There is no cross-method or cross-split-choice dedup/identity under `hash_value` alone. This is documented
  explicitly here, and again in [Open decisions and risks](#open-decisions-and-risks), as an accepted consequence of
  choosing an offset-manifest design, not an oversight.
* **ADR-0002's P2 `hash_policy`/`selection_rules`/discovery-endpoint vision is not built by this decision, and is now
  out of scope entirely** — not merely deferred, since there is no remaining algorithm choice to negotiate or
  discover once SHA-256 is the only primitive for both modes.
* **This ADR is `accepted` — the design is now implemented.** The gear computes both modes (whole-SHA-256 for
  non-multipart uploads, the multipart offset-manifest composite for multipart uploads), and `complete_multipart`
  builds the manifest + root from the already-collected per-part digests with no re-`GetObject` of the assembled
  object. The `version_hash_manifest` table, the `hash_mode`/`part_count` columns, the mode-aware
  `Store::verify_content_hash`, and the mode-aware `migrate_backend` all ship. Implementation was tracked in the P2
  remediation plan as Tier 4 item 4.6 ("BLAKE3 alignment" — the item name predates this simplification and refers to
  the offset-manifest design). See the [Confirmation](#confirmation) checklist below for the tests that back each
  claim.

### Confirmation

All confirmation items are satisfied by the shipped code and tests:

* [x] Code review confirming `StorageBackend::upload_part`/`complete_multipart` compute hashes on-the-fly from the
  streamed bytes and that no implementation re-reads the assembled/stored object to produce a hash. (`s3.rs`'s
  `complete_multipart` calls `finalize_multipart` — the POST-only helper — then `build_manifest_and_root`;
  `get_and_hash_streaming` was removed. `in_memory.rs` builds the manifest from the collected digests.)
* [x] Unit test confirming the manifest wire format is unambiguous: a fixed set of `(offset, digest)` pairs always
  serializes to the same expected byte string, and `sha256` of that string matches an independently-computed
  reference `root`. (`src/infra/content/hash_mode_tests.rs`)
* [x] Integration test asserting **no whole-object `GetObject`/re-read call** occurs at `complete_multipart` time —
  a request-counting wrapper backend that counts `get`/`get_stream` but deliberately not `get_range` (a bounded
  range read is not a "whole-object read" for this guarantee's purposes — see the test's own `CountingBackend` doc
  comment). This qualifier matters in practice: `complete_multipart_upload` does issue one small (~8 KiB) `get_range`
  against the assembled object for MIME magic-byte sniffing (P2 remediation item 1.10, added after this test), which
  is intentionally excluded from what this test guards.
  (`tests/content_hash_modes_test.rs::complete_multipart_issues_no_object_reread`)
* [x] Integration test asserting a client-side re-verification helper — split the object at the manifest's offsets,
  rehash each part, rebuild the manifest, compare to `root` — succeeds against real uploaded content and fails when
  any byte in any part is tampered with.
  (`tests/content_hash_modes_test.rs::client_reverification_succeeds_and_detects_tampering`)
* [x] Integration test asserting `migrate_backend` verifies a `multipart-composite-sha256` version correctly using
  only the object bytes and the stored `version_hash_manifest` row, with no read of `multipart_upload_parts` (the
  test deletes the parts rows first).
  (`tests/content_hash_modes_test.rs::migrate_backend_verifies_multipart_composite_without_parts_rows`)
* [x] Integration test asserting the finalize-time `expected_hash` check still rejects a client-claimed hash that does
  not match the sidecar's on-the-fly-computed value, for the `whole-sha256` mode. (`tests/finalize_test.rs` — the
  read-back-and-rehash mismatch path, unchanged.)

## Pros and Cons of the Options

### Exactly 2 modes, SHA-256-only, on-the-fly computation (chosen)

* Good, because it closes item 0.1 without the cost 0.1's current fix pays — no re-read at finalize/complete
* Good, because it introduces no second hash algorithm — no new dependency, no FIPS feature-gating machinery, no
  CMVP-discretion caveat
* Good, because the multipart mode's manifest is independently client-verifiable using nothing but a stock SHA-256
  implementation and the returned manifest text — no proprietary combination logic a third party must trust blindly
* Good, because it resolves the prior draft's biggest open question (must per-part digests be retained forever to
  re-verify composite-mode versions?) by construction — the manifest is the durable, self-contained record
* Good, because it imposes no part-size constraint on the multipart planner — arbitrary, even varying, part sizes
  work, unlike a BLAKE3-tree design's power-of-two requirement
* Good, because scope is minimal — two modes, zero configuration surface, no per-tenant routing machinery
* Bad, because the multipart digest is not comparable to a whole-object SHA-256 of the same content, and differs
  across different part-size choices for the same content — an explicit, accepted split-dependent-identity trade-off
  (see Open decisions)
* Bad, because it narrows the trust model to "trust the sidecar's on-the-fly computation" rather than "independently
  re-derive from storage alone" for the whole-object case — mitigated for the multipart case by the manifest making
  re-derivation from (object + manifest) fully independent of the sidecar's claim
* Bad, because manifest storage adds a new table and a bounded-but-nontrivial (~800 KB worst case) per-version storage
  cost for multipart uploads

### 3-mode design with BLAKE3-tree (prior draft, superseded before implementation)

This ADR's own earlier draft: non-multipart SHA-256 or BLAKE3 (selectable), multipart BLAKE3 canonical-tree
combination, multipart composite-SHA-256.

* Good, because BLAKE3-tree's digest is bit-identical to a plain `blake3::hash()` of the whole object — canonical,
  third-party-verifiable without any accompanying manifest
* Bad, because it requires a hard part-size constraint (uniform power-of-two-multiple-of-1024-byte parts) the current
  multipart planner does not satisfy and that has no compile-time enforcement against future drift
* Bad, because it introduces a second hash algorithm requiring Cargo feature gating mutually exclusive with
  `--features fips`, config-load-time rejection logic, and a `deny-fips.toml` interaction (BLAKE3 already sits in
  that file's Phase-B "future ban" backlog)
* Bad, because its own composite-SHA-256 mode (this ADR's mode 3) still carried the "must retain part digests
  forever" open question this simplified design resolves
* Rejected: never implemented; superseded by the offset-manifest design, which achieves the same on-the-fly,
  no-re-read, self-verifiable goals without a second algorithm or a part-size constraint

### ADR-0002's full `hash_policy`/selection-rules surface

Build the complete P2 vision ADR-0002 sketched: per-backend policy blocks, per-request negotiation, a
capability-discovery endpoint, per-tenant/mime/size-based `selection_rules`.

* Good, because it fully realizes ADR-0002's original design intent
* Bad, because nothing in the current codebase exercises any of this — a repo-wide grep for `hash_policy`/
  `allowed_algorithms`/`selection_rules` returns zero hits outside ADR/DESIGN prose
* Bad, because once SHA-256 is the only algorithm for both modes, there is no remaining algorithm choice for this
  surface to negotiate or discover — it has no work left to do, not merely no current requirement
* Rejected as out of scope; the two-mode decision here makes this surface's premise (a real algorithm choice to
  route) moot rather than merely deferred

### Keep the existing SHA-256-only, re-read at finalize

Do nothing further beyond item 0.1's already-shipped fix; leave the platform on whole-object SHA-256 with a
read-back-and-rehash at finalize/complete indefinitely.

* Good, because zero new work, zero new dependency, zero new schema
* Good, because it is the most conservative, best-understood option operationally
* Bad, because it never eliminates the finalize-time/complete-time re-read this ADR removes, leaving a known,
  quantifiable cost (a full re-read of every completed multipart object) on the table indefinitely
* Bad, because it leaves `DESIGN.md`'s stale multipart-hashing prose permanently incorrect against a codebase that
  will never implement any alternative, rather than resolving the discrepancy one way or the other
* Rejected: forecloses the re-read-elimination win without a technical blocker forcing that outcome, and the
  offset-manifest design achieves the same win without BLAKE3's costs

## More Information

### Open decisions and risks

Carried forward from `content-hash-modes.md` §11, not resolved by this ADR — flagged honestly rather than glossed over:

1. **Manifest storage size is bounded but not tiny.** Worst case ~10,000 parts × ~80 bytes/entry ≈ ~800 KB per
   manifest. This is why the manifest lives in its own table (not inline on `file_versions`), and why the multipart
   planner's minimum part size must continue to be enforced as a floor with a corresponding maximum part-count
   ceiling enforced independent of any one backend's native cap — a backend without S3's own part-count limit must
   not be allowed to silently produce an unbounded manifest.
2. **Split-dependent identity is a real, accepted trade-off.** The multipart mode's `root` is a hash of "content and
   split layout," not `sha256(whole bytes)`, so it differs from a single-part whole-object hash of the same content —
   cross-method dedup/identity does not hold, and two multipart uploads of the same content with different part-size
   choices also produce different digests. This is the direct, intentional cost of an offset-manifest design and is
   accepted here, not left as an unresolved question; any future feature wanting split-independent content addressing
   across multipart uploads would need a separately-computed mechanism, out of scope for this decision.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **ADR-0002** (superseded for P2+ content-hash-modes by this ADR): [Content Integrity Hash — SHA-256 in P1,
  Configurable in P2](./0002-cpt-cf-file-storage-adr-content-hash-selection.md)
- **ADR-0004**: [Signed-URL Token Format & Transport](./0004-cpt-cf-file-storage-adr-signed-url-transport.md) — the
  sidecar-as-trust-boundary posture this ADR's on-the-fly trust model relies on mirrors ADR-0004's "sidecar verifies,
  never the client" pattern
- **Design analysis**: [content-hash-modes.md](../features/content-hash-modes.md) — the full design document (manifest
  wire-format specification, schema DDL, trait signatures, staged implementation plan) this ADR formalizes

This decision directly addresses the following requirements or design elements:

* `cpt-cf-file-storage-adr-sidecar-data-plane` — the sidecar is the sole hash-computation site under the on-the-fly
  principle, and the sole trusted component in the reconciled trust model
* `cpt-cf-file-storage-fr-multipart-upload` — this ADR is the concrete P2+ multipart hash-mode decision that
  `cpt-cf-file-storage-fr-multipart-upload` needed once multipart upload became real
* `cpt-cf-file-storage-fr-backend-abstraction` — `StorageBackend::upload_part`/`complete_multipart` gain the
  `part_offset` parameter and manifest-building contract this decision requires
* `cpt-cf-file-storage-fr-metadata-storage` — the version row's `(hash_mode, hash_value)` plus the
  `version_hash_manifest` table remains the system-managed metadata this decision's verification model depends on
* `cpt-cf-file-storage-fr-get-metadata` — metadata responses (`VersionDto`, `GET /files/{id}/versions`) continue to
  surface the resolved hash fields, now including `hash_mode` and `part_count`. The `manifest` text itself is
  **not** part of this surface — it is returned only by the one-shot multipart `complete` response; see the tracked
  API-exposure gap under [Client and migrate_backend re-verification](#client-and-migrate_backend-re-verification)
