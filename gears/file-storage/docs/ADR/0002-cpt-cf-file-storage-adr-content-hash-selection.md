---
status: accepted
date: 2026-05-13
---

# ADR-0002: Content Integrity Hash — SHA-256 in P1, Configurable in P2

<!-- toc -->

- [Superseded Note (2026-07-07)](#superseded-note-2026-07-07)
- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [SHA-256 only (forever)](#sha-256-only-forever)
  - [BLAKE3 only (forever)](#blake3-only-forever)
  - [XXH3 only (forever)](#xxh3-only-forever)
  - [Custom server-side only (opaque, S3-ETag style)](#custom-server-side-only-opaque-s3-etag-style)
  - [Configurable on server and client, phased (chosen)](#configurable-on-server-and-client-phased-chosen)
- [More Information](#more-information)
  - [Client-side measurements (this benchmark)](#client-side-measurements-this-benchmark)
  - [Server-side measurements (public sources)](#server-side-measurements-public-sources)
  - [Why XXH3 (not XXHash64)](#why-xxh3-not-xxhash64)
  - [Why not BLAKE3 in P1](#why-not-blake3-in-p1)
  - [Sources](#sources)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-file-storage-adr-content-hash-selection`

## Superseded Note (2026-07-07)

> **ADR-0002's P1 decision is untouched by this note.** SHA-256-only in P1, shipped with the full `hash_policy`/preference/discovery API surface, remains the accurate historical record of what P1 decided and why — nothing below rewrites that accepted decision.
>
> **ADR-0002's P2 vision is superseded by [ADR-0006: Content-Hash Modes — Whole-Object SHA-256 & Multipart Offset-Manifest Composite (SHA-256)](./0006-cpt-cf-file-storage-adr-content-hash-modes.md).** The `hash_policy` config block (`default_algorithm`, `allowed_algorithms`, `selection_rules`), the capability-discovery endpoint, and the BLAKE3 "combine per-chunk chunk states into the tree root" finalization narrative described below in Decision Outcome and Consequences were **never implemented** and are **not** the P2+ plan going forward. ADR-0006 (`status: accepted` — its design is implemented) narrows and formalizes the P2+ content-hash decision to exactly **two** modes, both built on **SHA-256 alone — no second algorithm, no BLAKE3**: non-multipart whole-object SHA-256, and multipart SHA-256 **offset-manifest composite** (a canonical manifest of per-part offsets and SHA-256 digests, with `root = sha256(manifest)`, independently re-verifiable by any holder of the object bytes plus the returned manifest). Both modes are computed **on-the-fly during upload, never by re-reading the stored object** (the pre-existing single-part finalize read-back is retained for defense-in-depth; see ADR-0006). Because both modes use only SHA-256, there is no FIPS feature-gating, no Cargo feature flag, and no non-approved-algorithm carve-out to reason about. Read ADR-0006 for the authoritative P2+ decision; treat this ADR's P2 prose (Decision Outcome's second half, and the "P2 …" bullets in Consequences) as **historical vision that was consciously not built**, not a live plan.
>
> **Adversarial-tampering claim, refined.** The Context and Problem Statement below states that content-hash verification covers "both accidental corruption ... and adversarial tampering." ADR-0006 refines this for the P2+ modes it introduces: **only whole-object SHA-256** — via the whole-object hash and the server-side `expected_hash` re-verification path — carries an adversarial-integrity claim in this design. The multipart **offset-manifest composite** mode is explicitly scoped to **identity / accidental-corruption / correct-split-and-upload detection only**, the same non-adversarial treatment this ADR already gives XXH3 (see "P2 XXH3 enablement" in Consequences and the "XXH3 only (forever)" pros/cons below) — even though, unlike XXH3, the composite is itself built entirely from SHA-256 primitives; it is the bespoke *combination*, not the underlying algorithm, that keeps it out of the adversarial-integrity scope. There is no BLAKE3 or other non-approved algorithm anywhere in ADR-0006's design, so the FIPS 140-3 IG 2.4.A carve-out ADR-0006 previously needed to justify a non-approved algorithm no longer applies — see ADR-0006's FIPS Handling section, which is now trivial by construction.

## Context and Problem Statement

FileStorage routes all file content through its data-plane sidecar (`cpt-cf-file-storage-adr-sidecar-data-plane`). Every byte transits the sidecar, so content integrity verification — covering both accidental corruption (network, disk, broken intermediaries) and adversarial tampering — is a first-order requirement. The service must commit to a hash strategy that:

1. Works for **P1** (current scope), where uploads are single-part and the algorithm needs to be predictable, FIPS-acceptable, and operationally boring.
2. Extends cleanly to **P2**, where multipart upload (`cpt-cf-file-storage-fr-multipart-upload`) lands and the algorithm choice starts to interact with server-side multipart finalization cost, per-tenant policy, per-file-size routing, and storage-backend trust models.

Three algorithms are in scope as cryptographic / fast-hash candidates: **SHA-256**, **BLAKE3**, and **XXH3** (the modern xxHash family member; not XXHash64, which is older and slower on the same hardware). Each is well-characterized by both browser-side benchmarking (`tmp-sha-benchmark/results.csv`, `tmp-sha-benchmark/hash.md` — 991 single-threaded measurement rows across 19 implementations at 1 / 10 / 50 / 100 MB inputs) and public server-side benchmarks (cited in *More Information*).

The decision is whether to commit to a single algorithm forever, to commit to a single algorithm only in P1 and open the choice in P2, or to expose configuration to operators and clients from day one.

## Decision Drivers

* Phasing — P1 has no multipart upload, so the multipart-finalization cost differential (the load-bearing argument for BLAKE3) does not yet apply; P2 changes that and a strategy that is right for P1 may be wrong for P2 if it forecloses options
* Operational conservatism in P1 — a single, FIPS-acceptable, universally available default is the lowest-risk shipping choice for the first release
* Heterogeneous backends and workloads in P2 — different storage backends serve different trust models, throughput profiles, and regulatory regimes; a single global algorithm forces every backend to optimize for the strictest tier
* Client capability asymmetry — SHA-256 is free in browser WebCrypto (no ship cost) and on every server; BLAKE3 and XXH3 require a WASM gear (~75 KB and ~10 KB respectively) on browser clients but are dramatically faster server-side with SIMD
* Server-side multipart cost — for sequential hashes (SHA-256, XXH3) multipart finalization requires a streaming pass over the assembled object; BLAKE3's tree mode eliminates this pass by combining per-chunk states
* Business-logic axes for P2 — the right algorithm for a given upload may depend on the user, the file size, the storage configuration, or the tenant's policy; baking the choice into one global default removes that flexibility
* Client agency in P2 — clients with WASM/native runtimes should be able to express a preference (e.g. "I have BLAKE3, please use it for this multipart upload") subject to server-side allow-listing
* FIPS / regulatory acceptability — some tenants cannot use non-FIPS hashes; SHA-256 is the only FIPS-approved option in the candidate set

## Considered Options

* SHA-256 only (single algorithm, forever)
* BLAKE3 only (single algorithm, forever)
* XXH3 only (single algorithm, forever)
* Custom server-side only (opaque server-defined, S3-ETag style)
* Configurable on server and client (per-backend policy + per-request client preference), **phased — SHA-256 fixed in P1, configurable in P2**

## Decision Outcome

Chosen option: "Configurable on server and client, phased — full configuration surface from P1, allow-list expanded in P2". **For P1 the only available algorithm is SHA-256.**

**P1** (current scope, no multipart upload): ship the **full hash-selection API surface from day one** — backend policy block (`default_algorithm`, `allowed_algorithms`, `selection_rules`), client preference parameter on upload requests, capability discovery endpoint reporting which algorithms a backend allows. The **only value in the allowed-algorithm set is SHA-256**. Backend default is `SHA-256`. Client preference is accepted on every request but, since the allow-list contains only SHA-256, the effective resolved algorithm in P1 is always SHA-256. SHA-256 is computed on the proxy streaming path during upload and verified on download. The result hash and the resolved algorithm identifier are stored as system-managed metadata and returned to the client in the upload response and on every metadata query.

This shape — full API surface, locked allow-list — gives P1 the same client / SDK / wire contract that P2 will use, so when P2 expands the allow-list there is no breaking change for clients or storage backends: the response shape, the discovery endpoint, and the preference parameter all already exist. Operators who configure a new backend in P1 already write the policy block in its final shape; what changes in P2 is only which values are legal in `allowed_algorithms`.

**For P1 we choose SHA-256** as the single available algorithm because it is FIPS-approved, universally available (WebCrypto, every server runtime — zero ship cost on browser clients), naturally compatible with the S3-API facade's ETag-shaped expectations, and operationally boring; and because P1 has no multipart upload, the load-bearing argument for BLAKE3 (tree-mode finalization) does not yet apply.

**P2** (when multipart upload lands per `cpt-cf-file-storage-fr-multipart-upload`): keep the same API surface and expand the `allowed_algorithms` allow-list to include BLAKE3 and XXH3 in addition to SHA-256. The configuration semantics already shipped in P1 — backend default + allow-list + selection rules + client preference — now do real work:

* **Server side** — each storage backend gains a hash-policy configuration block: a `default_algorithm`, an `allowed_algorithms` allow-list, and a `selection_rules` section that maps business-logic predicates (user identity, file size buckets, mime-type classes, storage-tier flags) to a specific algorithm choice. The effective algorithm for a given upload is resolved server-side at request time by evaluating the selection rules and falling back to the default.
* **Client side** — clients may advertise a preferred algorithm in the upload request (e.g. a browser SDK that has shipped the BLAKE3 WASM gear asks for BLAKE3). The server treats the preference as a hint: if the preferred algorithm is in the backend's allowed list and consistent with the selection rules, it is used; otherwise the server resolves the effective algorithm independently and reports it back in the response.
* **Discovery** — clients can query a backend for its allowed-algorithm set so the SDK can avoid ship cost (e.g. skip loading BLAKE3 WASM if the active backend only allows SHA-256).

The result hash and the resolved algorithm identifier are stored as system-managed metadata (`cpt-cf-file-storage-fr-metadata-storage`) and returned on every metadata query so consumers can re-verify.

### Consequences

* **P1 deliverables**: hashing pipeline supports only SHA-256, but the **full API surface ships from P1** — backend abstraction (`cpt-cf-file-storage-fr-backend-abstraction`) accepts a `hash_policy` block; SDK upload methods accept an optional `hash_algorithm` preference parameter; capability discovery exposes the backend's `allowed_algorithms` set; the upload response and metadata responses carry the resolved `(hash_algorithm, hash_value)` pair. In P1 every backend's `allowed_algorithms` is locked to `["SHA-256"]` and every resolved algorithm is `SHA-256`; the configuration schema validator rejects any other value
* **P2 allow-list expansion**: the same configuration schema accepts BLAKE3 and XXH3 in `allowed_algorithms`; introducing the new values is a config change, not an API change — **no wire-format change, no client SDK breakage, no protocol bump**. It is, however, **not** migration-free at the database layer: the P1 `files` table locks `hash_algorithm` with `CHECK (hash_algorithm = 'SHA-256')` and `hash_value` with `CHECK (octet_length(hash_value) = 32)`, so P2 MUST run a DDL migration to widen both constraints (drop-and-re-add to `hash_algorithm IN ('SHA-256','BLAKE3','XXH3')` and the per-algorithm digest-length check). This migration is already authored in the P2 section of `migration.sql`. Existing P1 rows are untouched — it is a constraint widening, not a data backfill
* **P2 multipart finalization** — for BLAKE3-configured backends, finalization combines per-chunk BLAKE3 chunk states (32 bytes per part, carried over the proprietary protocol) into the tree root without re-streaming the assembled object; for SHA-256 and XXH3, finalization performs a streaming pass that the proxy already has on the upload streaming path
* **P2 XXH3 enablement** — XXH3 is non-cryptographic; selecting it in a backend's `allowed_algorithms` MUST require an explicit operator acknowledgement in the configuration tooling, and the backend's metadata responses MUST mark XXH3-hashed objects as corruption-detection-only (not integrity-against-adversaries)
* **P2 capability discovery** — `cpt-cf-file-storage-fr-backend-capabilities` is extended with a per-backend `supported_hash_algorithms` list; the *active* set per upload is resolved by the server-side selection rules and reported in the response
* **Forward compatibility** — because P1 already records the algorithm identifier alongside the hash bytes, P2 introduces no migration step for existing P1 objects; they remain valid SHA-256-hashed objects and may be re-hashed under another algorithm only on explicit cross-backend copy or migration
* **ETag header semantics** (`cpt-cf-file-storage-fr-conditional-requests`) remain decoupled from the content hash; the ETag is an opaque cache validator and clients MUST NOT assume it equals the content hash, regardless of phase
* **Any future S3-compatible facade gear** built on top of FileStorage — in P1 such a facade's content-integrity expectations would be naturally SHA-2-shaped; in P2 the facade would have to surface the active algorithm explicitly because S3 clients have strong assumptions about ETag derivation
* **FIPS posture** — preserved in P1 (SHA-256 only); in P2 deployments that require FIPS configure their backends with `default_algorithm: SHA-256` and `allowed_algorithms: [SHA-256]`, effectively locking the platform to P1 behavior

### Confirmation

P1 confirmation:

* Code review confirming that the backend abstraction accepts a `hash_policy` block, the SDK upload methods accept an optional `hash_algorithm` preference, and the capability discovery endpoint exists and returns the configured `allowed_algorithms` set
* Code review confirming that the configuration schema validator rejects any value in `allowed_algorithms` other than `SHA-256` in P1
* Integration tests verifying that uploads produce stable SHA-256 hashes matching an independent reference implementation
* Integration tests verifying that a client preference for an algorithm other than SHA-256 is rejected by the server with a clear error (because that value is not in any backend's allow-list) and that omitting the preference resolves to SHA-256 via the backend default
* Integration tests verifying that the upload response, the metadata response, and the capability discovery response all carry the resolved `(hash_algorithm, hash_value)` pair and `allowed_algorithms` set respectively, with `SHA-256` as the only value present

P2 confirmation (when P2 ships):

* Code review confirming that backend configuration accepts the hash-policy block and that the runtime resolves the effective algorithm via the selection rules
* Code review confirming that XXH3 enablement in the configuration tooling requires an explicit operator acknowledgement
* Integration tests per algorithm verifying that:
  * Single-part upload produces a stable hash that matches an independent reference implementation
  * For BLAKE3, a multipart upload produces the same root hash as the same file uploaded as a single part
  * For SHA-256 and XXH3, multipart finalization produces the correct whole-file hash via the streaming pass
* Client-server negotiation tested end-to-end: client preference honored when in allow-list, ignored with explicit fall-back reporting when not
* Documentation surfaces the trade-off table (SHA-256 / BLAKE3 / XXH3) and the XXH3 non-cryptographic caveat in operator-facing docs

## Pros and Cons of the Options

### SHA-256 only (forever)

Single global algorithm. No configuration, no choice, no negotiation.

* Good, because zero configuration surface — no operator decisions, no client preference plumbing, no XXH3 footgun
* Good, because FIPS-approved by default — every deployment is regulator-acceptable without further thought
* Good, because universally available on every client (WebCrypto, every server runtime) — no extra WASM or library to ship
* Good, because S3-compatible facade interop is straightforward (SHA-2-shaped expectations match)
* Bad, because forecloses BLAKE3 adoption forever, including for deployments where the multipart-finalization cost is operationally meaningful
* Bad, because every backend pays the SHA-256 cost even when only accidental-corruption detection is needed at multi-GB/s — no way to opt down
* Bad, because in P2 the server-side multipart finalization requires a streaming pass; this is fine when it can be folded into the upload streaming path the proxy already has, but it leaves no room to reclaim that cost on BLAKE3-friendly workloads

### BLAKE3 only (forever)

Single global algorithm. Every upload, every backend, every client uses BLAKE3.

* Good, because tree-mode multipart finalization eliminates the streaming re-pass and the server-side cost differential at fleet scale is large (orders of magnitude in CPU; entire bandwidth class eliminated when otherwise re-reading cross-region)
* Good, because cryptographically secure (256-bit collision and preimage resistance, same nominal level as SHA-256)
* Good, because client and server can compute the root hash in parallel — end-to-end verification possible without an extra round-trip
* Bad, because not FIPS-approved — regulated tenants cannot use the platform without falling back to SHA-256 anyway, so a "BLAKE3 only" rule is unenforceable in practice
* Bad, because not available in WebCrypto — browser SDKs must ship a ~75 KB WASM gear on every page load
* Bad, because younger algorithm with less cumulative cryptanalysis vs SHA-256 (academic concern; no practical attacks today)
* Bad, because removes the ability to offer XXH3 for trusted high-throughput internal workloads where the operator has explicitly accepted the corruption-only model

### XXH3 only (forever)

Single global algorithm. Every upload uses XXH3.

* Good, because XXH3 is the fastest by a wide margin — native AVX2 measurements in the public benchmarks reach ~31 GB/s, with reports of 50+ GB/s on optimized AVX2 paths; on our client-side benchmark XXH3 (via hash-wasm) is the second-fastest entry at ~6.1 GB/s, behind only XXHash64
* Good, because cheap to compute on tiny inputs — useful when metadata operations dominate
* Bad, because not cryptographic — collisions can be deliberately constructed by an attacker who controls input; cannot serve as content-addressable hash, signature input, or adversarial integrity check
* Bad, because suitable only for accidental-corruption detection; using it as the *only* integrity mechanism leaves the platform unable to claim adversarial-tamper resistance for any tenant
* Bad, because not FIPS, not standardized in the cryptographic sense, and unsuitable for any compliance regime that requires named hash families
* Bad, because the multipart finalization remains sequential — same re-pass cost class as SHA-256, without any of SHA-256's regulatory benefits

### Custom server-side only (opaque, S3-ETag style)

The platform does not commit to any standard hash; each backend returns an opaque content identifier whose derivation is internal and may change.

* Good, because zero ship cost on clients — no hashing happens client-side; whatever the server computes is what the response carries
* Good, because the platform retains full control over the on-disk hash format and may swap algorithms internally without changing the client contract
* Good, because tracks the historical S3 ETag pattern (MD5 of single-part, `md5(concat(md5(part_i))) + "-N"` for multipart) — familiar to operators
* Bad, because gives up end-to-end verifiability — the client cannot independently verify the server's claim because it does not know how the value was computed
* Bad, because content-addressable use cases (deduplication, signing, cross-system references) become impossible without exposing the algorithm
* Bad, because the S3 ETag history is a cautionary tale: same file, different part size → different ETag; consumers built workarounds because the format leaked implementation details despite being "opaque"
* Bad, because forecloses any future where clients want to participate in hashing (e.g. computing BLAKE3 root locally during upload for end-to-end check)

### Configurable on server and client, phased (chosen)

The full configuration / negotiation surface ships in P1 (backend `hash_policy`, client preference parameter, capability discovery), but P1 locks every backend's allow-list to `["SHA-256"]`. P2 expands the allow-list to include BLAKE3 and XXH3 and gives the selection rules real work to do — no API or schema change.

* Good, because the API contract clients and operators learn in P1 is the same one P2 uses; expanding the allow-list in P2 is a configuration change, not a breaking API change
* Good, because the on-disk format already carries the algorithm identifier from P1, so P2 introduces no migration step for objects hashed under P1
* Good, because P1 still ships the most conservative algorithm choice (SHA-256) — the surface exists, but the only legal value is the safe one; bad operator decisions are not yet possible
* Good, because P2 lets operators match algorithm to backend reality — regulated tenants stay on SHA-256, multipart-heavy backends move to BLAKE3, trusted high-throughput internal backends opt into XXH3 with explicit acknowledgement
* Good, because per-request client preference makes the SDK ergonomic without forcing the server's hand — the server is always the authority on what is allowed
* Good, because business-logic selection rules (by user, by file size, by storage policy) let a single backend serve heterogeneous traffic profiles without proliferating backend configurations
* Good, because preserves FIPS posture for tenants who need it (allow-list `[SHA-256]` is the P1 default and a perfectly valid P2 configuration) and offers BLAKE3 / XXH3 for tenants who do not
* Bad, because P1 ships configuration / negotiation machinery that does nothing observable in P1 — the cost of the API surface (config-schema validation logic, the preference parameter, the discovery endpoint) is paid before the benefit is available, and operators learning the surface before it does anything may form incorrect mental models. The payoff is narrower than a blanket "no breaking change in P2": the *wire and config contract* is preserved, but P2 still requires a DDL constraint-widening migration (see Consequences → P2 allow-list expansion), so the surface earns its keep on API stability, not on a zero-migration claim
* Bad, because P2 introduces real choices with real consequences; bad selection rules or careless XXH3 enablement create observable footguns once the allow-list is open
* Bad, because the platform must maintain three hasher implementations (SHA-256, BLAKE3, XXH3) and three multipart finalization strategies (streaming pass, tree-mode combine, streaming pass) once P2 lands
* Bad, because the client-server negotiation surface is a long-term API contract that constrains future changes (e.g. introducing a fourth algorithm)

## More Information

### Client-side measurements (this benchmark)

From `tmp-sha-benchmark/hash.md` and `tmp-sha-benchmark/results.csv`, single-threaded MB/s averaged over 1 / 10 / 50 / 100 MB inputs. Relevant rows for this decision:

| Algorithm | Implementation | Group | Avg MB/s |
|---|---|---|---:|
| XXHash3 | hash-wasm | wasm | 6 135 |
| SHA-256 | WebCrypto | native (SHA-NI) | 1 191 |
| BLAKE3 | hash-wasm | wasm | 508 |
| SHA-256 | hash-wasm | wasm (software fallback) | 221 |
| BLAKE3 | @noble/hashes | purejs | 39 |

Reads:

* In the browser on SHA-NI-capable hardware, **native SHA-256 (~1.2 GB/s) beats WASM BLAKE3 (~0.5 GB/s)** by roughly 2.3× — the WebCrypto path is hard to beat because the browser has the hardware accelerator and BLAKE3 / XXH3 do not.
* Without SHA-NI, WASM BLAKE3 (~0.5 GB/s) wins over software SHA-256 (~0.2 GB/s) by roughly 2.3×, and XXH3 in WASM (~6 GB/s) is in a different league entirely but is non-cryptographic.
* The implication for the client side is that BLAKE3 / XXH3 are useful precisely where the operator wants to *avoid* the SHA-NI path (e.g. older hardware, software-only builds), or where the multipart-tree property matters more than the single-stream throughput.

### Server-side measurements (public sources)

Server-side single-threaded native (compiled) numbers, drawn from the public benchmarks cited under *Sources*:

| Algorithm | Server single-thread native | Notes |
|---|---|---|
| SHA-256 (SHA-NI) | ~2–3 GB/s | OpenSSL `speed` on recent x86_64 with SHA-NI; ARMv8 with crypto extensions comparable |
| SHA-256 (software, no SHA-NI) | ~0.5–0.8 GB/s | Reference OpenSSL software path |
| BLAKE3 (AVX2) | ~6.4–8.4 GB/s | Official BLAKE3 Rust crate reference numbers |
| BLAKE3 (AVX-512) | ~12 GB/s | Same crate, AVX-512 build |
| BLAKE3 (16 cores, 1 GB file, rayon) | ~92 GB/s | Multi-thread scaling, ~11× single-thread |
| XXH3 (scalar) | ~8.4 GB/s | xxHash reference docs |
| XXH3 (AVX2) | ~31 GB/s | xxHash reference docs, large inputs |
| XXH3 (AVX2, peak optimized) | ~50–59 GB/s | Reports from XXH3 author and downstream users |

Reads:

* Server-side, BLAKE3 single-thread overtakes SHA-256-SHA-NI by ~2–3× and the multi-core BLAKE3 scaling is the **largest single throughput lever** in the candidate set; the multipart-finalization cost story (BLAKE3 tree mode → no re-pass) compounds this on top of the raw throughput advantage.
* Server-side, XXH3 is order-of-magnitude faster than either crypto hash, which is why it is the right pick for trusted high-throughput corruption-detection workloads but the wrong pick anywhere an adversarial input is plausible.
* The relative ordering server-side is **inverted** from the client-side WebCrypto situation: on the server BLAKE3 wins on speed; in the browser SHA-256 wins on speed thanks to WebCrypto + SHA-NI. The configurable design (P2) lets each side use the algorithm that suits *that* runtime's strengths.

### Why XXH3 (not XXHash64)

The earlier draft of this ADR considered XXHash64 as the non-cryptographic option. XXH3 is the modern xxHash family member (released 2019, stabilized in xxHash 0.8.0) and is strictly preferred for new code:

* Faster than XXHash64 on all input sizes — order-of-magnitude faster on large inputs thanks to AVX2 / AVX-512 paths
* Better small-input performance via the dedicated short-input code path
* Same non-cryptographic caveat applies; XXH3 is not crypto-secure and the same operator acknowledgement is required

The replacement is purely an upgrade within the same "fast non-crypto hash" slot.

### Why not BLAKE3 in P1

P1 has no multipart upload. The load-bearing argument for BLAKE3 — tree-mode multipart finalization eliminating the server-side re-pass — does not apply yet. Without that lever, BLAKE3's other properties (~2× the cost of native SHA-256 in the browser; not FIPS; ~75 KB WASM ship cost) are net-negative for a first release. The right time to introduce BLAKE3 is when the property that justifies it becomes operationally meaningful, which is in P2 alongside multipart upload.

### Sources

* Browser-side measurements: this repository's `tmp-sha-benchmark/results.csv` and `tmp-sha-benchmark/hash.md`.
* [BLAKE3 official Rust and C reference (GitHub)](https://github.com/BLAKE3-team/BLAKE3) — single-thread AVX2 / AVX-512 numbers, multi-core scaling.
* [BLAKE3 specification PDF](https://raw.githubusercontent.com/BLAKE3-team/BLAKE3-specs/master/blake3.pdf) — tree-mode construction, security argument.
* [xxHash project benchmarks](https://xxhash.com/) — XXH3 scalar and AVX2 throughput.
* [XXH3 — a new speed-optimized hash algorithm (fastcompression blog)](http://fastcompression.blogspot.com/2019/03/presenting-xxh3.html) — XXH3 design notes.
* [SHA-NI / OpenSSL multi-buffer SHA-256 reference (OpenSSL repo)](https://github.com/openssl/openssl/blob/master/crypto/sha/asm/sha256-mb-x86_64.pl) — server-side SHA-256 path.
* [SHA-Intrinsics (Noloader)](https://github.com/noloader/SHA-Intrinsics) — Intel / ARMv8 / Power8 SHA intrinsics for native SHA-256.
* [SHA-256 Alternatives 2025: BLAKE3 vs SHA-3 vs xxHash3 (devtoolspro.org)](https://devtoolspro.org/articles/sha256-alternatives-faster-hash-functions-2025/) — comparative server benchmarks across the three families.

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **ADR-0003**: [Split the Data Plane into a Signed-URL Sidecar](./0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-file-storage-adr-sidecar-data-plane` — Because all content traffic transits the sidecar, hashing happens on the sidecar streaming path; the algorithm choice is a platform decision rather than a backend-native concern
* `cpt-cf-file-storage-fr-multipart-upload` — P2 multipart upload is the trigger for opening the algorithm choice; tree-mode finalization for BLAKE3 backends interacts with the multipart wire protocol
* `cpt-cf-file-storage-fr-backend-abstraction` — P2 backend abstraction surfaces the configured `hash_policy` (default, allow-list, selection rules) so that upload / download paths route hashing through the matching implementation
* `cpt-cf-file-storage-fr-backend-capabilities` — P2 extends per-backend capability declaration with `supported_hash_algorithms`
* `cpt-cf-file-storage-fr-metadata-storage` — System-managed metadata stores the `(hash_algorithm, hash_value)` pair on every object, even in P1 where `hash_algorithm` is fixed at `"SHA-256"`, so the schema is forward-compatible
* `cpt-cf-file-storage-fr-get-metadata` — Metadata responses return the `(hash_algorithm, hash_value)` pair so consumers can verify integrity by re-hashing
* `cpt-cf-file-storage-fr-content-type-validation` — Independent of hash selection; mime detection runs on the same proxy streaming path
* `cpt-cf-file-storage-fr-conditional-requests` — ETag semantics remain decoupled from content hash; the ETag is opaque and clients MUST NOT assume it equals the content hash
* Any future S3-compatible facade gear — would have to surface the active algorithm explicitly because S3 clients have strong ETag assumptions
