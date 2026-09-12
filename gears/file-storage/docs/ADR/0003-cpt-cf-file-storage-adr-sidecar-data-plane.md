---
status: accepted
date: 2026-06-16
---

# ADR-0003: Split the Data Plane into a Signed-URL Sidecar

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Single monolith proxies all content (prior proxy-all design)](#single-monolith-proxies-all-content-prior-proxy-all-design)
  - [Direct-to-backend presigned URLs (rejected by the prior proxy-all design)](#direct-to-backend-presigned-urls-rejected-by-the-prior-proxy-all-design)
  - [Signed-URL sidecar data plane (chosen)](#signed-url-sidecar-data-plane-chosen)
- [More Information](#more-information)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-file-storage-adr-sidecar-data-plane`

## Context and Problem Statement

the original proxy-all design made FileStorage a single
in-process monolith through which **every byte of every upload and download** flows. It chose this
over direct-to-backend presigned URLs to preserve backend opacity, per-byte metering, and uniform
audit / policy coverage. the prior proxy-all design consciously accepted the cost: FileStorage becomes a
terabyte-scale data-plane bottleneck whose bandwidth — not CPU or memory — is the binding capacity
constraint (`cpt-cf-file-storage-nfr-bandwidth`).

That cost is the problem. Coupling the control logic (auth, metadata, policy, versioning,
conditional requests) to the byte-moving data plane in one process means the whole stack must scale
on the bandwidth dimension, the data plane cannot be moved closer to heavy consumers or the edge
without relocating the control logic with it, and a slow client streaming a 50 GB object ties up a
process slot that also serves cheap metadata reads.

The question is whether the byte-moving data plane can be separated from the control plane
**without** giving up the properties the prior proxy-all design was protecting (backend opacity, central metering,
uniform enforcement) — i.e. without falling back to the direct-to-backend presigned-URL model that
the prior proxy-all design rejected.

## Decision Drivers

* Backend opacity — clients must never depend on a backend protocol surface (S3, Azure Blob REST,
  WebDAV) or learn a backend-addressable URL (`cpt-cf-file-storage-principle-backend-opacity`)
* Centralized per-byte metering for usage reporting (`cpt-cf-file-storage-fr-usage-reporting`) and
  uniform content-type validation, audit, and read audit on 100% of transfers — no per-flow
  carve-outs
* Independent scaling of the data plane — byte-moving capacity must scale (and relocate to the edge)
  without dragging the control logic and its database connections along
* Operability of long transfers — a slow or large transfer must not consume a control-plane request
  slot or hold a metadata-DB connection
* A single client protocol regardless of backend, so SDKs and any future facade gears stay uniform
* The cost the prior proxy-all design accepted (`cpt-cf-file-storage-nfr-bandwidth`) should be confined to the part of
  the system that actually moves bytes, not the part that makes decisions

## Considered Options

* Single monolith proxies all content (the prior proxy-all status quo)
* Direct-to-backend transfer via presigned URLs (the option the prior proxy-all design rejected)
* **Signed-URL sidecar data plane** — split into a control plane that issues signed URLs and a
  data-plane sidecar that moves the bytes

## Decision Outcome

Chosen option: **"Signed-URL sidecar data plane"**. FileStorage is split into two cooperating
planes:

* **Control plane** — the FileStorage **API / SDK**. Owns metadata, authorization, versioning, and
  conditional-request semantics. **Its HTTP REST surface never accepts or returns file content.**
  It issues short-lived **signed URLs** that point at the sidecar. The only path where control-side
  code touches bytes is the in-process **SDK proxy mode**, which streams inside the *consumer
  gear's* process — never through the control-plane service.
* **Data plane** — the **sidecar**. It has its own domain and URL and is the only component that
  moves user bytes. It is connected to N storage backends and validates the signed-URL signature
  (and a platform token only when the signed URL carries a token-claim predicate — see DESIGN §3.2).
  **Shipped path (P2):** the sidecar reaches the control plane over a **plain, token-authenticated
  HTTP callback** — `POST .../versions/{version_id}/finalize` after a successful `PUT`, and
  `POST .../multipart/{upload_id}/parts/{n}/report` after a successful part write — both authorized
  solely by the **same signed `fs-token`** that authorized the original operation. There is no FS SDK
  s2s call, no app-token, and no on-behalf-of delegation: the control plane treats a verified token as
  full authorization for that one `(file_id, version_id)` operation. The sidecar holds **no** direct
  database connection and is a thin, stateless byte-mover. It never binds a version as the file's
  current content — `bind` (the CAS swap of `content_id`) is a separate, later request the **client**
  issues directly to the control plane. *Amendment (upload-flow redesign):* this remains true of the
  sidecar itself, but the **control plane's finalize handler** may now perform the bind as part of the
  same finalize transaction when the upload token carries the `bind_on_finalize` claim — minted only by
  `POST /files` with `bind: "auto"` for a brand-new file's first content, executed under a strict
  `content_id IS NULL` CAS, and reported back through the sidecar as opaque `X-FS-Bound`/`ETag`
  response headers. A deliberate, narrow extension of the same delegated-authorization model: the
  sidecar still holds no DB access and makes no bind decision — it forwards headers it never
  interprets; replacing existing content still requires the JWT-authorized `bind`/`complete` paths. This trades a little per-op latency (the finalize round-trip)
  for a clean security/failure boundary: the data plane never gets DB credentials and cannot mutate
  metadata except through the control plane's own, independently-verifying handlers. The alternative
  **direct-DB** mode — the sidecar as a full FileStorage instance over the shared metadata DB (lower
  latency, no control round-trip) — remains a deferred, unscheduled co-located-deployment optimization.
  The control plane pre-registers the version itself, in `pending` status, in the same request that
  returns the signed PUT URL — never via a later sidecar-initiated call.
  > **Implemented as above (P2).** An earlier P1 draft of this ADR instead described the sidecar
  > reaching the control plane over the FS SDK in s2s REST mode, with its own app-token plus an
  > on-behalf-of `<user>` claim, to pre-register *and auto-bind* a version. That delegation model was
  > never built.

The critical difference from the direct-to-backend model the prior proxy-all design rejected: **the signed URL points
at our own sidecar, never at the raw backend.** Therefore every property the prior proxy-all design protected is
retained — they simply move into the sidecar, which is platform-controlled infrastructure:

* backend opacity — the client only ever talks to the sidecar; backend identity, native URLs, and
  protocol never leak;
* per-byte metering, content-type validation, audit, and read audit run on 100% of transfers, in
  the sidecar, with no per-flow carve-outs;
* a single client protocol independent of backend.

A **read** is therefore two HTTP requests: a control request (obtain a signed GET URL) plus a data
request against the sidecar. A **write** is more: **presign (control) → `PUT` (data) → finalize
(data→control, a token-authenticated callback) → `bind` (control)** — three control-plane touches and
one data-plane touch, not the two-request model this ADR originally described (see the Implementation
note above).

Signed URLs are **Ed25519, stateless** (S3-presigned-style): the control plane signs with a private
key and is the **sole minter**; the sidecar verifies with the public key and can never forge a URL.
Constraints are AND-combined into the signed payload — `exp` (required, capped at a configured
`max_url_ttl`, recommended 7 days, enforced by the control plane at signing), optional
`ip`/CIDR, optional predicates over token claims (`tok.typ`, `tok.sub`, `tok.tenant_id`, …), and — on
upload URLs — an optional size bound (`max_size` or `exact_size`, mutually exclusive) and
`expected_hash`. Bandwidth (`max_rate`) and connection (`max_conns`) caps are declared but enforced in
P2. In P1 there is one static keypair (private in control config, public in sidecar config);
rotation/keyset is deferred to P2; there is no per-URL revocation — emergency revocation is the
platform auth module's token revocation, not the URL layer.

### Consequences

* The bandwidth cost the prior proxy-all design accepted (`cpt-cf-file-storage-nfr-bandwidth`) is now confined to the
  sidecar. The data plane scales (and can be relocated to the edge / co-located with heavy
  consumers) by adding stateless sidecar replicas, independently of the control plane. The
  DESIGN.md "bandwidth escape hatch" stops being a thought experiment and becomes the architecture.
* `cpt-cf-file-storage-fr-rest-api` is restated: the control-plane REST surface carries metadata and
  signed URLs only; **content endpoints live on the sidecar**, addressed by signed URL.
* Content I/O becomes a multi-request sequence, reflected in every upload/download sequence
  (`cpt-cf-file-storage-seq-*`): reads are presign + transfer; writes are presign → `PUT` → finalize
  (data→control callback) → `bind` (see the Implementation note above). Upload uses an **immutable-blob + pointer** model:
  content is written to an immutable backend object `/{file_id}/{version_id}`; the file's current
  content is a DB pointer (`content_id`) swapped under optimistic CAS — see DESIGN §4.x and
  `cpt-cf-file-storage-fr-upload-file`. A new version is a new object plus a pointer swap; backend
  content is never mutated in place (enforced, not just conventional: the sidecar's single-shot
  publish is create-exclusive — see DESIGN.md's `PUT`-token-replay consequence note — so a second
  `PUT` to an already-published path is rejected rather than silently replacing it).
* `cpt-cf-file-storage-fr-content-type-validation`, `cpt-cf-file-storage-fr-usage-reporting`, and
  `cpt-cf-file-storage-fr-read-audit` are reallocated from the monolith to the sidecar; coverage
  stays 100%.
* `cpt-cf-file-storage-fr-range-requests` are served by the sidecar; a single signed download URL
  serves many `Range` requests (random access) because the `Range` header is not part of the
  signature.
* The sidecar's finalize/report-part callbacks to the control plane are authorized solely by the same
  signed `fs-token` that authorized the original operation — **not** by an app-token / on-behalf-of
  delegated identity (see the Implementation note above). The sidecar never performs the `bind`
  (version) write itself; that remains a client-issued, user-authorized control-plane request
  (`cpt-cf-file-storage-fr-authorization`).
* **Trust model update (P2 remediation 0.1, remaining half).** The `fs-token` alone is
  client-visible: it is handed back to the client in plaintext inside `upload_url` (minted by
  `sign_url`), so a client could always call `finalize`/`report-part` itself at a time of its
  choosing, replay reports, or otherwise occupy the trust position the callback was designed for the
  sidecar alone. The data-integrity half of this was already closed independently (the control plane
  re-derives `size`/`hash`/`mime_type` from a real streaming read-back — see
  `domain/service/write.rs::read_back_and_hash_streaming` — so a forged claim cannot corrupt stored
  metadata). What remained was *who* is allowed to call these two routes at all. The chosen
  mechanism is an **interim gear-local shared secret** (not the platform's
  `toolkit-security::internal_auth` profiles, which are not yet deployable in this gear — Profile 1
  is in-process-only trust, useless across the sidecar/control-plane process boundary; Profile 2
  (`BootstrapToken`) is struct-only with validation deferred; Profile 3 needs K8s `TokenReview`
  wiring this gear doesn't have): `FileStorageConfig::finalize_internal_secret` (optional) plus
  `require_finalize_internal_secret` (fail-fast startup guard, mirroring `require_signing_key_seed`).
  When configured, `finalize`/`report-part` additionally require a `x-fs-internal-token` header
  matching the configured secret (constant-time comparison via `ring::constant_time`,
  `handlers::FinalizeAuth`), checked *after* `fs-token` verification; a missing/mismatched header is
  a `403`. The sidecar sends this header (from `FS_SIDECAR_INTERNAL_TOKEN`) on both callbacks when
  configured; unset on either side preserves pre-0.1 behavior (token-only trust), so **the rollout
  order matters**: (1) deploy the control plane with the secret configured but
  `require_finalize_internal_secret: false` (accepts calls with or without the header); (2) redeploy
  every sidecar talking to it with the matching `FS_SIDECAR_INTERNAL_TOKEN`; (3) only then flip
  `require_finalize_internal_secret: true` (rejects any caller lacking the header, closing the
  client-driven-finalize gap). Flipping step 3 before step 2 completes bricks uploads from any
  not-yet-redeployed sidecar. This is explicitly a stop-gap: once the platform's `internal_auth`
  profiles are deployable here, `handlers::FinalizeAuth`'s comparator should be swapped for
  `InternalAuthenticator` and this shared secret retired.
* **Known gap: multipart part-hash trust.** The data-integrity claim above
  ("a forged claim cannot corrupt stored metadata") only holds for the
  **single-shot** `PUT`/finalize path, where `read_back_and_hash_streaming`
  re-derives `size`/`hash`/`mime_type` from the real backend bytes. For
  **multipart** uploads there is no equivalent re-read: `report_part`
  persists the caller-supplied part hash after only a length/size check (not
  a re-hash of the bytes actually written), and `complete_multipart_upload`
  builds the composite `hash_value`/manifest exclusively from those stored
  per-part hashes (ADR-0006) — the assembled object itself is never re-hashed
  end to end. Since the `fs-token` authorizing a part write is client-visible
  (the same exposure this bullet's trust-model update addresses) and the
  `x-fs-internal-token` gate is not required by default
  (`require_finalize_internal_secret: false`), a caller holding a valid part
  token could in principle report a hash that does not match the bytes it
  streamed, corrupting the composite hash without being caught by any
  read-back. Mitigations available today: enable `finalize_internal_secret` +
  `require_finalize_internal_secret` so only the sidecar (not an arbitrary
  token holder) can reach `report_part`/`finalize` at all — this gate already
  covers `report_part`, not just `finalize` (`handlers::report_multipart_part`
  calls the same `FinalizeAuth::verify`). A durable fix (deriving the part
  hash from a sidecar-side value the control plane can independently trust,
  or re-hashing the assembled object) is future work, out of scope for this
  remediation. A related gap in the same release gate is now closed in code for every shipping
  backend, though the closure differs by backend: `StorageBackend::publish_exclusive`'s **default**
  implementation (`infra/backend/mod.rs`) is a non-atomic (TOCTOU) `exists`-then-`put`, but no
  shipping backend relies on that default. `LocalFsBackend` (`std::fs::hard_link`, which atomically
  fails `AlreadyExists` if the target already exists) and `InMemoryBackend` (a single mutex guarding
  both the check and the insert) each override it with a fully atomic, provider-independent
  implementation; `S3Backend` **overrides** it with an atomic conditional write (`If-None-Match: *`
  on the terminal `PutObject`/`CompleteMultipartUpload`, mapping S3's `412 Precondition Failed` to a
  `created: false` outcome). Unlike the other two, S3's guarantee is **provider-dependent**: it holds
  only against an endpoint that honours S3 conditional writes (native AWS S3 since 2024-08, and
  S3-compatible stores that implement it); confirming a specific target deployment actually enforces
  the precondition — rather than silently ignoring the header and degrading to last-write-wins — is
  a required check before S3 leaves its ADR-0005 release gate, same as the part-hash trust gap above.
* A new signed-URL contract (`cpt-cf-file-storage-fr-signed-urls`) and the constraint model become
  part of the public surface; the response-header set the sidecar must echo verbatim is baked into
  the signed URL.
* The OoP/gRPC SDK escape hatch in `cpt-cf-file-storage-constraint-toolkit-gear` is reframed: an
  out-of-process caller is handed a signed URL (or proxied via the sidecar), not streamed through
  the control plane.

### Confirmation

Implementation verified via:

* Code review confirming the control-plane REST surface neither reads nor writes a request/response
  body containing file content (only metadata + signed URLs).
* Code review confirming the sidecar is the only component that opens backend clients for content
  I/O, and that no signed URL or SDK return value exposes a backend-addressable URL to a client.
* Code review confirming the control plane is the sole signer (holds the Ed25519 private key) and
  the sidecar only verifies (holds the public key).
* Integration tests covering presign → transfer for upload and download, the bind/rebind CAS path
  (including the failed-precondition retry that does not re-upload bytes), and signed-URL constraint enforcement
  (expiry, ip, token-claim predicates).
* Usage reports include per-byte ingress/egress counters emitted by the sidecar.

## Pros and Cons of the Options

### Single monolith proxies all content (prior proxy-all design)

* Good, because one component, one deployment, no signed-URL machinery, no two-request dance
* Good, because all of the prior proxy-all design's properties hold trivially (everything is in one place)
* Bad, because control logic and the byte data plane scale on the same (bandwidth) dimension
* Bad, because the data plane cannot be relocated to the edge or a heavy consumer without moving the
  control logic and its DB connections with it
* Bad, because long/slow transfers consume control-plane request slots and DB connections

### Direct-to-backend presigned URLs (rejected by the prior proxy-all design)

* Good, because FileStorage carries no content bandwidth at all
* Bad, because the client must speak N backend protocols; backend identity leaks through the URL;
  per-byte metering, audit, and content validation fragment into per-flow carve-outs — the exact
  reasons the prior proxy-all design rejected it. **Not reconsidered here.**

### Signed-URL sidecar data plane (chosen)

* Good, because it keeps every property the prior proxy-all design protected (backend opacity, central metering,
  uniform enforcement) — the signed URL points at our sidecar, not the backend
* Good, because the data plane scales and relocates independently of the control plane
* Good, because the control plane stays thin: metadata + authz + signed URLs, no byte streaming, no
  request slot held for the duration of a transfer
* Good, because the immutable-blob + pointer model makes versioning backend-agnostic and makes
  conflict retries cheap (re-bind a `version_id`, never re-upload bytes)
* Bad, because a read is at least two HTTP requests (presign + transfer) and a write is more
  (presign → `PUT` → finalize callback → `bind`)
* Bad, because it introduces signed-URL machinery (signing, verification, key distribution, the
  constraint model) and a token-authenticated callback path (the sidecar's finalize/report-part calls
  back to the control plane)
* Bad, because there are now two deployable units (control plane + sidecar) and a shared metadata DB
  contract between them

## More Information

This realizes, as the baseline architecture, the "full-FileStorage-instance escape hatch" that
the prior proxy-all design's accepted-cost discussion (`cpt-cf-file-storage-nfr-bandwidth`,
`cpt-cf-file-storage-topology-overview`) described as a future option. The sidecar is not a
byte-mover trait extracted from the monolith; it is a full FileStorage data plane over the shared
(or remote) metadata DB, which is why it needs no wire-contract change to relocate.

ADR-0002 (content hash selection: SHA-256 in P1, configurable in P2) is unaffected — hashing still
happens on the streaming path, now in the sidecar.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related**: [ADR-0002: Content Integrity Hash](./0002-cpt-cf-file-storage-adr-content-hash-selection.md)

> Supersedes the original proxy-all monolith design (its ADR was removed; see `git log` for the history).

This decision directly addresses the following requirements or design elements:

* `cpt-cf-file-storage-fr-rest-api` — control REST carries metadata + signed URLs only; content lives on the sidecar
* `cpt-cf-file-storage-fr-signed-urls` — new: the Ed25519 stateless signed-URL contract and constraint model
* `cpt-cf-file-storage-fr-upload-file` / `cpt-cf-file-storage-fr-download-file` — presign + transfer (download: two requests; upload: presign → `PUT` → finalize → `bind`); immutable-blob + pointer model
* `cpt-cf-file-storage-fr-range-requests` — served by the sidecar; one signed URL, many ranges
* `cpt-cf-file-storage-fr-usage-reporting`, `cpt-cf-file-storage-fr-content-type-validation`, `cpt-cf-file-storage-fr-read-audit` — reallocated to the sidecar, still 100% coverage
* `cpt-cf-file-storage-nfr-bandwidth`, `cpt-cf-file-storage-nfr-scalability` — the bandwidth dimension is confined to the independently-scaled sidecar
* `cpt-cf-file-storage-fr-authorization` — the sidecar's finalize/report-part callbacks are authorized solely by the
  signed `fs-token` (no app-token / on-behalf-of delegation); `bind` remains a client-issued, user-authorized request
