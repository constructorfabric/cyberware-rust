---
status: proposed
date: 2026-07-07
---

# ADR-0005: S3 Client Selection

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [`rusty-s3` (+ `quick-xml`) — chosen](#rusty-s3--quick-xml--chosen)
  - [`object_store` (Apache Arrow, `aws` feature) — fallback](#object_store-apache-arrow-aws-feature--fallback)
  - [`aws-sdk-s3` (+ `aws-config`)](#aws-sdk-s3--aws-config)
  - [`rust-s3`](#rust-s3)
  - [Raw HTTP (no S3 client)](#raw-http-no-s3-client)
- [More Information](#more-information)
  - [Option Comparison](#option-comparison)
  - [Binary-size impact (measured)](#binary-size-impact-measured)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-file-storage-adr-s3-client-selection`

## Context and Problem Statement

Tier 1 item 1.7 of the P2 remediation plan ("No durable/distributed storage backend (S3) despite doc claims")
requires an `S3Backend` implementing the `StorageBackend` trait (`src/infra/backend/mod.rs`), the seam every backend
type must satisfy. Before this ADR, the crate shipped two backend types: `LocalFsBackend` (sets `durable: true` —
writes survive a process restart on a single machine — but has no native multipart support) and `InMemoryBackend`
(sets `multipart_native: true`, but is explicitly non-durable — content is lost on restart). Neither, on its own,
survives a **distributed, multi-replica** deployment without an externally-shared mount: `local-fs` durability is
real but confined to whatever filesystem the process sees, and `in-memory` durability is deliberately absent
altogether. `S3Backend` is needed to close that distributed-durability gap.

An `S3Backend` needs, at minimum, the following operations, all invoked from **async** (tokio) code:

* `PutObject`
* `GetObject`
* `GetObject` **with `Range`** (backs `StorageBackend::get_range`, native-range reads instead of the default
  read-whole-then-slice fallback)
* `HeadObject` (backs `StorageBackend::size` / `StorageBackend::exists` without materializing the blob)
* `DeleteObject`
* `ListObjectsV2` **with continuation-token pagination** (backend enumeration)
* Multipart: `CreateMultipartUpload`, `UploadPart`, `CompleteMultipartUpload`, `AbortMultipartUpload` (item 1.7.4,
  large-object streaming upload)

Two properties of this gear narrow the option space considerably:

1. **Presigned URLs are not needed.** `cpt-cf-file-storage-adr-signed-url-transport` (ADR-0004) already gives the gear
   its own Ed25519 signed-URL format, minted by the control plane and verified by the sidecar; the sidecar is the only
   component that ever holds S3 credentials and calls S3 directly with them. An S3 client's presigning surface — a
   headline feature of most S3 SDKs — is therefore irrelevant to this decision.
2. **`reqwest` 0.13 (rustls 0.23) is already a dependency** of this crate — it is what the sidecar uses today for its
   own HTTP surface. Any S3 client that executes HTTP requests itself brings its *own* HTTP/TLS stack; if that stack
   is not `reqwest`/`rustls`, the binary ends up linking two independent HTTP clients and (often) two TLS
   implementations. This is a real, measurable cost (binary size, two sets of CVEs to track, two connection-pool
   configurations to tune), not just an aesthetic one — and it is why "sign raw `reqwest` calls ourselves, no
   dedicated S3 client" is a legitimate option here, not a strawman.

One more scoping note: `StorageBackend::id`/`list_paths`-style enumeration in this crate returns a plain
`Vec<String>` of *all* matching paths — there is no continuation-token parameter exposed to `StorageBackend` callers.
An S3 abstraction that paginates `ListObjectsV2` internally and hands back a fully-drained list (or an async stream
the caller can drain fully) is therefore sufficient; we do not need raw access to `ListObjectsV2`'s continuation
token.

This ADR chooses which S3 client crate backs `S3Backend`.

## Decision Drivers

* **Avoid duplicating the HTTP/TLS stack** already present (`reqwest` 0.13 / rustls 0.23) — a second stack is a
  concrete binary-size, maintenance, and security-surface cost, not a style preference
* **Binary size / dependency weight** — net-new transitive dependencies pulled into every build of the gear
* **Native coverage of the required operations** — especially `Range` `GetObject`, paginated `ListObjectsV2`, and all
  four multipart operations, without hand-rolled request construction
* **Maintenance / community health** — release cadence, maintainer count, issue responsiveness; this becomes the
  gear's exposure if the crate stalls
* **Security-review burden for an external SDK** — `src/infra/backend/mod.rs:11` already flags that "S3/GCS/etc. are
  deferred (they require an external SDK + security review)"; ADR-0003 establishes the same posture (external
  dependencies that touch the data plane get reviewed, not rubber-stamped)
* **FIPS posture** — consistency with the rustls / `aws-lc-rs` posture the rest of the gear is converging on (see
  ADR-0004's FIPS discussion for the signed-URL codec); a client that hard-wires a different TLS/crypto backend works
  against that convergence
* **Execute vs. sign-only** — whether the crate performs the HTTP call itself or only produces a signature/request the
  caller must execute (and, if sign-only, how much response parsing/error-handling is left to us)
* **In-house maintenance burden** — how much S3 wire-protocol code (XML parsing, error mapping, retry/backoff) this
  gear would own indefinitely if the crate does not provide it

## Considered Options

1. **`aws-sdk-s3`** (+ `aws-config`) — the official AWS SDK for Rust
2. **`object_store`** (Apache Arrow, `aws` feature) — a generic multi-cloud object-store abstraction
3. **`rust-s3`** — a community S3 client
4. **`rusty-s3`** — a minimal, sign-only S3 request-signing crate, paired with `quick-xml` for response parsing
5. **Raw HTTP, no S3 client** — `reqwest` (already present) + `aws-sigv4` (official SigV4 signing) + `quick-xml`
   (response parsing), fully hand-rolled

Research gathered July 2026; all five are actively maintained, with commits within roughly the last two weeks of the
research date.

## Decision Outcome

Chosen option: **`rusty-s3` (+ `quick-xml`)** — `rusty-s3`'s presigned-request builder to construct each S3 call,
executed over our existing `reqwest` 0.13 HTTP client, with S3's XML responses and error bodies parsed in-house via
`quick-xml` (pinned `>= 0.41.0`), behind `S3Backend: StorageBackend`.

Against the drivers above:

* It is the **smallest real-S3-API footprint** that adds **no second HTTP/TLS stack**: `rusty-s3` only builds
  presigned requests, and we execute them with the `reqwest` 0.13 client this crate already ships — measured
  **+11.7% binary size / +25 transitive crates** (see [Binary-size impact](#binary-size-impact-measured) below).
* It **covers all 10 required operations** via its request-builder: `PutObject`, `GetObject`, `GetObject` with
  `Range`, `HeadObject`, `DeleteObject`, `ListObjectsV2`, `CreateMultipartUpload`, `UploadPart`,
  `CompleteMultipartUpload`, `AbortMultipartUpload`.
* We accept its costs deliberately: a **small community** (152★, BSD-2-Clause license); a signing API that is
  **presigned-URL-only** — it has no Authorization-header (execute-now) signing mode — but a presigned SigV4 URL is a
  legitimate way to make the sidecar's direct-to-S3 calls (the sidecar executes the presigned request itself, it is
  never handed to an untrusted client), and a `Range` header layers on top of the presigned URL without issue since
  `Range` is not part of SigV4's signed canonical request; and **S3's response XML and error schema are parsed
  in-house** with `quick-xml`, a real ongoing maintenance item rather than something the crate provides for us.
* **`quick-xml` must be pinned `>= 0.41.0`**: two DoS advisories, RUSTSEC-2026-0194 and RUSTSEC-2026-0195, are fixed
  only as of that release.

**Fallback / documented alternative:** `object_store`. If `rusty-s3`'s DIY XML-parsing/error-mapping surface or its
presigned-only signing model proves insufficient in practice, or the team wants a fully-executing, multi-backend
client with a larger maintenance base behind it, switch to `object_store`. Its size position today is worse than
`rusty-s3`'s — the current crates.io release (`0.14`) pulls in `reqwest` 0.12, duplicating the HTTP/TLS stack
alongside our `reqwest` 0.13 (measured **+40.0% / +45 crates**) — but its `main` branch has already moved to
`reqwest` 0.13; a future release (approximately `0.15`) collapses that duplication, at which point we **estimate**
(not measured — roughly 1 MB of the current delta is the duplicated stack) it would cost **approximately +30–35
crates / +0.35–0.5 MB (approximately +10–13%)**, making it roughly size-equivalent to `rusty-s3` while bringing a
stronger community and an executing (not sign-only) client. That is the basis for holding it as the ready fallback
rather than a rejected option.

**Also considered and retained as documented alternatives, not the primary choice:**

* **`aws-sdk-s3`** remains the heavyweight **official** alternative if first-party AWS support or full API
  completeness is ever required — measured **+130.4% / +155 crates** (it drags in STS, SSO, SSO-OIDC, and the
  smithy runtime alongside `s3` itself).
* **`rust-s3`** is **rejected**: it pins `reqwest` 0.12 with default `native-tls`, duplicating the HTTP/TLS stack, and
  is maintained by a single maintainer — measured **+41.4% / +60 crates**.
* **Raw HTTP** (`aws-sigv4` + `quick-xml`, no S3 client at all) remains the lightest-weight **escape hatch** —
  measured **+5.6% / +49 crates**, the smallest binary-size delta of all five — if dependency budget ever becomes the
  overriding constraint. It is noted here rather than chosen because it is strictly *more* in-house DIY work than
  `rusty-s3` (request construction as well as response parsing) for comparatively little additional size saving
  (+5.6% vs. `rusty-s3`'s +11.7%).

**Security-review gate.** Per `src/infra/backend/mod.rs:11` ("S3/GCS/etc. are deferred (they require an external SDK
+ security review)") and the external-dependency review posture ADR-0003 establishes for data-plane-adjacent code,
whichever client is ultimately vendored — `rusty-s3` (+ `quick-xml`) per this decision, or `object_store` if the team
later invokes the documented fallback — **must clear a team security review before being merged as a real
dependency**. The team has selected `rusty-s3` as the S3 client pending that review; this ADR records the comparison
and the team's decision, but does not itself constitute the review. That is why this ADR's status is `proposed`, not
`accepted`. The same gate also covers `S3Backend`'s create-exclusive `publish_exclusive` override (see Consequences
below): it is implemented and opt-in (S3 itself is only used when a deployment configures `s3_backends`), but its
atomic-write guarantee is provider-dependent, and confirming that a specific target endpoint actually honours
`If-None-Match: *` conditional writes — rather than silently ignoring the header — is a required part of this
review before S3 leaves the gate (see also ADR-0003's "Known gap" discussion of the same override).

**Current implementation status (2026-07-08).** The branch contains working S3 backend code and runtime dependency
wiring so integration tests and operator config can exercise the selected design. This does **not** close the gate
above: before merging to `main` or enabling S3 in a release path, attach the team security-review artifact for
`rusty-s3` and `quick-xml` (including XML parsing/DoS considerations and CVE ownership) and then update this ADR's
status to `accepted`. If that review is not complete, S3 must remain out of the merge/release path or be feature-gated
off.

### Consequences

* Two new runtime dependencies, `rusty-s3` and `quick-xml` (pinned `>= 0.41.0`), are added to `file-storage`'s
  sidecar-side crate (the only component that opens backend clients for content I/O, per ADR-0003). `rusty-s3` only
  builds presigned requests; we execute them with the crate's existing `reqwest`/rustls/`aws-lc-rs` stack, so no new
  TLS backend or HTTP client is introduced network-wide. `S3Backend: StorageBackend` (item 1.7.1) wraps `rusty-s3`'s
  bucket/credentials/action builders, translating `StorageBackend`'s path/range/multipart vocabulary onto presigned
  `PutObject`/`GetObject`/`HeadObject`/`DeleteObject`/`ListObjectsV2`/multipart-action URLs that this gear executes
  via `reqwest` and whose XML responses it parses via `quick-xml`.
* `StorageBackend::get_range` gets a real native implementation for the S3 backend: `rusty-s3` builds the presigned
  `GetObject` URL, and the gear layers an unsigned `Range` header onto the executed request (valid because `Range` is
  not part of SigV4's signed canonical request), instead of falling back to the default whole-object-read-then-slice.
  `BackendCapabilities::range_native = true` for `S3Backend`.
* `StorageBackend::size`/`exists` are backed by a presigned `HeadObject` request, avoiding a full-object `get` per the
  trait's documented intent.
* Multipart (item 1.7.4) is implemented via `rusty-s3`'s `CreateMultipartUpload`/`UploadPart`/
  `CompleteMultipartUpload`/`AbortMultipartUpload` presigned-request builders, executed via `reqwest`, with
  `quick-xml` parsing `CompleteMultipartUpload`'s XML response body; `S3Backend`'s `BackendCapabilities::
  multipart_native = true`, unblocking the multipart-coordinator's S3 path noted in the plan's Tier 1 item 1.7
  multipart discussion.
* Enumeration (`list_paths`-style callers) is implemented by manually paginating `rusty-s3`'s presigned
  `ListObjectsV2` requests, extracting the continuation token from each `quick-xml`-parsed response and looping until
  exhausted, then flattening into `StorageBackend`'s contractual `Vec<String>`. Unlike an auto-paginating client
  abstraction, this pagination loop is code this gear owns and tests directly.
* `StorageBackend::publish_exclusive` (the sidecar's single-shot upload path, ADR-0003) is implemented for `S3Backend`
  as an atomic conditional write: `If-None-Match: *` on the terminal `PutObject`/`CompleteMultipartUpload`, mapping
  S3's `412 Precondition Failed` to a `created: false` outcome instead of the trait's non-atomic `exists`-then-`put`
  default. Like the rest of `S3Backend`, this is opt-in (only exercised once a deployment configures `s3_backends`)
  and its guarantee is provider-dependent — see the Security-review gate above and ADR-0003's "Known gap" discussion.
* **In-house XML/error-handling burden.** Because `rusty-s3` is sign-only, this gear — not the crate — owns parsing
  every S3 XML response body (`ListObjectsV2`, `CompleteMultipartUpload`) and mapping S3's XML error schema to
  `StorageBackend`'s error types, via `quick-xml`. This is an accepted, explicit trade-off for zero HTTP/TLS stack
  duplication, not a hidden one, and it is a real, ongoing maintenance item (including tracking `quick-xml`'s own
  CVEs, as the pinned-version note above already reflects).
* This is still a **new external dependency pair** regardless of which of the five options is chosen; it does not
  skip the security-review gate flagged in `src/infra/backend/mod.rs:11`, and this ADR cannot itself close that gate.
* **FIPS posture.** Executing `rusty-s3`'s presigned requests over our existing `reqwest`/rustls/`aws-lc-rs` chain
  means no *second* crypto/TLS backend is introduced — consistent with the FIPS-posture direction ADR-0004 sets for
  the rest of the gear (route through a single, swappable, eventually FIPS-validatable module rather than accumulate
  multiple hard-wired crypto stacks). It does **not** by itself make the gear FIPS-compliant: no independent FIPS
  validation of `rusty-s3`'s SigV4 signing implementation or `aws-lc-rs`'s request-signing path is claimed here, only
  that choosing it avoids adding a *second, divergent* TLS/crypto surface on top of the one the rest of the gear
  already uses.

### Confirmation

* A completed security review of `rusty-s3` and `quick-xml` (pinned `>= 0.41.0`) — the concrete crates this ADR
  recommends — covering their transitive dependency trees, licensing (`rusty-s3`: BSD-2-Clause; `quick-xml`: MIT,
  both compatible), and known advisories (confirming RUSTSEC-2026-0194 and RUSTSEC-2026-0195 are fixed at the pinned
  `quick-xml` version).
* Code review confirming `S3Backend` sets `durable: true` (already set by `LocalFsBackend`) **and**
  `multipart_native: true` (already set by `InMemoryBackend`) — the first backend type to combine both in a way that
  also survives a distributed, multi-replica deployment — and that it implements `get_range` and `size`/`exists`
  natively rather than via the trait's default (whole-object) fallbacks.
* Confirming, against the specific target S3-compatible endpoint(s) a deployment intends to use, that conditional
  writes (`If-None-Match: *`) are actually enforced rather than silently ignored — required before `S3Backend`'s
  `publish_exclusive` override can be relied on for the create-exclusive guarantee ADR-0003 describes.
* `cargo tree` (or equivalent) run against the crate with `rusty-s3` and `quick-xml` added, confirming no second
  `reqwest`/`hyper`/TLS major version is pulled in beyond what the crate already links.
* Integration tests (item 1.7's test strategy, `s3s-fs`-backed) covering: `put`/`get` round-trip, `Range` `get`,
  `head`-based `size`/`exists`, `delete`, paginated `list` over more objects than one `ListObjectsV2` page, the full
  multipart lifecycle (`CreateMultipartUpload` → `UploadPart` × N → `CompleteMultipartUpload`, plus
  `AbortMultipartUpload` on a cancelled upload), and at least one test exercising the in-house `quick-xml` parsing of
  an S3 XML error response.

## Pros and Cons of the Options

### `rusty-s3` (+ `quick-xml`) — chosen

152★ (`rusty-s3`). 4,288 SLOC. Latest `0.10.0` (2026-06-18). Paired with `quick-xml` (20,977 SLOC excluding tests;
1,534★; pinned `>= 0.41.0`) for XML response/error parsing.

* Good, because it is **sign-only** (no HTTP execution of its own), so it reuses our `reqwest` 0.13 / rustls 0.23 /
  `aws-lc-rs` stack with **zero HTTP/TLS stack duplication** — measured the **smallest real-S3-API footprint** of the
  five: +11.7% binary size, +25 net-new transitive crates
* Good, because it shares provenance with the official `aws-sigv4`/smithy ecosystem for its signing correctness
* Good, because its request-builder natively covers all 10 required operations (`Put`/`Get`/`Get`+`Range`/
  `Head`/`Delete`/`ListObjectsV2`/`Create`/`Upload`/`Complete`/`Abort`Multipart)
* Bad, because its signing API is **presigned-URL-only** — it has no Authorization-header (execute-now) signing mode;
  we accept this because a presigned SigV4 URL executed directly by the sidecar (never handed to an untrusted client)
  is a legitimate substitute, and `Range` layers on top of the presigned URL without re-signing since `Range` is not
  part of SigV4's signed canonical request
* Bad, because **all** response XML parsing and S3 error-schema handling is left to us via `quick-xml` — a real,
  ongoing in-house maintenance burden, including tracking `quick-xml`'s own CVEs (two DoS advisories,
  RUSTSEC-2026-0194 and RUSTSEC-2026-0195, were fixed only as of this month's `0.41.0` release — the version this ADR
  mandates pinning)
* Neutral, because its smaller community (152★, BSD-2-Clause) is proportionate to its narrower scope (a signing
  library, not a full client), not necessarily a red flag on its own, though it is the smallest review surface of the
  five candidates

### `object_store` (Apache Arrow, `aws` feature) — fallback

293★ on the dedicated `object_store` repository (split out of `apache/arrow-rs`, 3,520★, in 2025; same maintainers).
25,363 SLOC total (4,664 AWS-specific). Latest `0.14.0` (2026-06-22).

* Good, because it is an **executing client** (performs the HTTP call itself, not merely sign-only) and it **natively
  covers every required operation**: `put`, `get`/`get_opts` (byte range for `Range` `GetObject`), `head`, `delete`,
  an auto-paginating `list` stream (`ListObjectsV2` under the hood), and a `put_multipart` trait exposing `put_part`/
  `complete`/`abort` (the four multipart operations) — no in-house XML parsing or error mapping required
* Good, because it is maintained by the **Apache Arrow project** (293★ standalone, but the same maintainer pool as
  the 3,520★ `apache/arrow-rs`), a materially larger review/maintenance base than `rusty-s3`'s
* Good, because its generic, multi-backend design (S3/GCS/Azure/local) mirrors this gear's own `BackendRegistry` /
  multi-backend-type architecture (`cpt-cf-file-storage-fr-backend-abstraction`), so the abstraction level is a
  natural fit rather than an impedance mismatch
* Good, because RUSTSEC-2024-0358 is fixed in the current version
* Bad, because its current crates.io release (`0.14`) pins `reqwest` **0.12**, duplicating the HTTP/TLS stack
  alongside our `reqwest` 0.13 (confirmed via `cargo tree`) — measured **+40.0% binary size, +45 net-new crates**,
  the reason it is the fallback rather than the primary choice today
* Neutral, because its `main` branch has already moved to `reqwest` 0.13; once that ships in a release (estimated
  `~0.15`), the stack-duplication cost collapses to an **estimated** (not measured) +30–35 crates / +0.35–0.5 MB
  (+10–13%), making it roughly size-equivalent to `rusty-s3` while keeping the stronger community and executing-client
  properties above — the point at which re-evaluating it as primary becomes attractive
* Bad, because its abstraction hides raw `ListObjectsV2` continuation tokens behind an auto-paginating stream — a real
  gap for a caller that needs manual page-by-page control, though **not one this gear has**, since
  `StorageBackend`'s own enumeration contract already returns a flat, fully-drained list
* Bad, because it is still a new external dependency requiring the security review flagged in
  `src/infra/backend/mod.rs:11` — fallback status is not a review exemption

### `aws-sdk-s3` (+ `aws-config`)

The official AWS SDK for Rust. 3,328★ (`aws-sdk-rust`). ~192,753 generated Rust SLOC across the `s3` and
`aws-config` crates. Latest `1.137.0` (2026-06-16).

* Good, because it is the **official** AWS SDK, with first-party support and the broadest possible native operation
  coverage (every S3 API, not just the subset this gear needs)
* Good, because it has the largest community and longest track record of the five
* Bad, because it executes HTTP via **`hyper`**, not `reqwest`, and pulls in **both `hyper` 0.14 and 1.x, and both
  rustls 0.21 and 0.23** — a duplicate HTTP/TLS stack alongside our existing `reqwest` 0.13 / rustls 0.23, with no
  first-party `reqwest` connector (only the unofficial, separately-maintained `aws-smithy-http-client-reqwest`)
* Bad, because it is by far the heaviest option measured: **+130.4% binary size, +155 net-new crates** (it drags in
  STS, SSO, SSO-OIDC, and the smithy runtime alongside `s3` itself)
* Bad, because RUSTSEC-2023-0125 (an `aws-sigv4` credential leak via `TRACE`-level logging) affected this SDK's
  signing crate; it is fixed in the current version, but it is a reminder that a large official SDK is not
  automatically a smaller review surface than a smaller crate
* Neutral, because its presigning feature set — a major reason teams pick an official SDK — is irrelevant here (see
  Context)
* Neutral, because it remains a documented heavyweight alternative if first-party AWS support/completeness is ever a
  hard requirement, not a live recommendation today

### `rust-s3`

670★. 9,009 SLOC. Latest `0.37.2` (2026-05-04).

* Good, because it has explicit `list_page()` support for manual `ListObjectsV2` continuation-token pagination
* Bad, and **disqualifying**: it pins `reqwest` **0.12** with default `native-tls`, which compiles a second major
  version of `reqwest` alongside our 0.13 — each with its own `hyper` and TLS stack (confirmed via `cargo tree`).
  This is precisely the stack duplication the decision drivers rule out — measured **+41.4% binary size, +60 net-new
  crates**
* Bad, because it is maintained by a single maintainer, the smallest review/continuity guarantee among the executing
  clients

### Raw HTTP (no S3 client)

`reqwest` (already present) + `aws-sigv4` (official SigV4 header signing; 3,918 SLOC, shares the smithy-rs repo with
`aws-sdk-s3`) + `quick-xml` (XML parsing; 20,977 SLOC excluding tests; 1,534★, pinned `>= 0.41.0`).

* Good, because it has **zero HTTP/TLS stack duplication** — it is built directly on the `reqwest` we already ship —
  and measured the **smallest binary-size delta of all five options**: +5.6% binary size, +49 net-new crates
* Good, because it gives full control over request construction, retries, and backoff, with no intermediate
  abstraction to work around
* Good, because `aws-sigv4` is the same official, well-reviewed signing crate `aws-sdk-s3` itself uses
* Bad, because **100% of S3 request construction, response XML parsing, error-schema mapping, retries, and pagination
  is hand-rolled and owned by this gear indefinitely** — by far the largest in-house maintenance burden of the five
  options, strictly more DIY work than `rusty-s3` (which at least supplies the request-construction/signing half) for
  comparatively little additional size saving over it (+5.6% vs. `rusty-s3`'s +11.7%)
* Bad, because `quick-xml` requires active CVE tracking: two DoS advisories, RUSTSEC-2026-0194 and RUSTSEC-2026-0195,
  were fixed only as of the `0.41.0` release this month — **any adoption of this path must pin `quick-xml` >= 0.41.0**
* Neutral, because it remains the lightest-weight *escape hatch* if dependency budget ever outweighs the in-house
  maintenance cost — noted as a documented fallback-of-the-fallback, not a live recommendation

## More Information

### Option Comparison

✓ = yes / good · ✗ = no / bad

| Candidate | Stars | Rust SLOC | Executes? | Stack vs our reqwest 0.13 | Net-new crates (measured) | Release binary Δ (measured) | Verdict |
|---|---|---|---|---|---|---|---|
| **`rusty-s3` (+ `quick-xml`)** | 152 | 4,288 | ✗ (sign-only, presigned-only) | **Identical** (no execution) | +25 | +11.7% (+413,792 B) | **Chosen** |
| `object_store` (`aws`) | 293† | 25,363 (4,664 AWS) | ✓ | Duplicates today (reqwest 0.12); collapses once on 0.13 | +45 | +40.0% (+1,408,960 B) | Fallback |
| `aws-sdk-s3` (+`aws-config`) | 3,328 | ~192,753 | ✓ | Duplicates (hyper 0.14+1.x, rustls 0.21+0.23) | +155 | +130.4% (+4,592,400 B) | Heavyweight alternative |
| `rust-s3` | 670 | 9,009 | ✓ | Duplicates (reqwest 0.12, native-tls) | +60 | +41.4% (+1,457,664 B) | Rejected |
| Raw HTTP (`aws-sigv4`+`quick-xml`) | — | 3,918 + 20,977 | ✓ (via our reqwest) | Identical | +49 | +5.6% (+198,704 B) | Escape hatch |

† `object_store`'s dedicated repository star count; its parent `apache/arrow-rs` (same maintainers) shows 3,520★.

### Binary-size impact (measured)

**Measurement method.** Sizes were measured with a dedicated probe crate built under an aggressive profile
(`opt-level = 3, lto = true, codegen-units = 1, strip = true, panic = "abort"`) — more aggressive than this gear's
actual release profile (`codegen-units = 1, panic = "unwind"`, line-tables debug info, no `lto`/`strip`). Absolute
sizes below are therefore smaller than a real gear build would produce, but the *deltas* between candidates — each
measured as "baseline + exactly one candidate added" — are a fair relative comparison, which is what this decision
turns on.

| Candidate | Release binary size | Δ vs. baseline | Crates (total) |
|---|---|---|---|
| Baseline (tokio + reqwest 0.13, no S3 client) | 3,521,904 bytes (~3.52 MB) | — | 138 |
| **`rusty-s3` (+ `quick-xml`)** | 3,935,696 bytes | +413,792 B (+11.7%) | +25 (163) |
| `object_store` (`aws` feature, current `0.14`) | 4,930,864 bytes | +1,408,960 B (+40.0%) | +45 (183) |
| `aws-sdk-s3` (+`aws-config`) | 8,114,304 bytes | +4,592,400 B (+130.4%) | +155 (293) |
| `rust-s3` | 4,979,568 bytes | +1,457,664 B (+41.4%) | +60 (198) |
| Raw HTTP (`aws-sigv4` + `quick-xml`) | 3,720,608 bytes | +198,704 B (+5.6%) | +49 (187) |

These are the measured numbers underlying the Decision Outcome above. The ADR's `status` remains `proposed` not
because sizes are unmeasured, but because the security review required by `src/infra/backend/mod.rs:11` / ADR-0003
has not yet run against the chosen `rusty-s3` + `quick-xml` pair (see [Confirmation](#confirmation)).

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Remediation plan**: Tier 1 item 1.7, "No durable/distributed storage backend (S3) despite doc claims"
- **Related**: [ADR-0003: Split the Data Plane into a Signed-URL Sidecar](./0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)
- **Related**: [ADR-0004: Signed-URL Token Format & Transport](./0004-cpt-cf-file-storage-adr-signed-url-transport.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-file-storage-fr-backend-abstraction` — `S3Backend` is a new `StorageBackend` implementation, chosen client
  determines how cleanly it fits the trait's `put`/`get`/`get_range`/`size`/`exists`/`delete`/multipart surface
* `cpt-cf-file-storage-fr-backend-capabilities` — `S3Backend` sets both `durable: true` (also set by
  `LocalFsBackend`) and `multipart_native: true` (also set by `InMemoryBackend`); it is the first backend to combine
  both while also surviving a distributed, multi-replica deployment
* `cpt-cf-file-storage-nfr-bandwidth` (ADR-0003) — the sidecar is the sole component that opens the chosen S3 client
  and moves bytes against it; the client choice does not change the sidecar/control-plane split
* `cpt-cf-file-storage-adr-signed-url-transport` (ADR-0004) — confirms the chosen client's presigning features are
  unused; the gear's own Ed25519 signed URLs remain the only credential surface exposed outside the sidecar
