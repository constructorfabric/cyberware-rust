---
status: accepted
date: 2026-06-20
---

# ADR-0004: Signed-URL Token Format & Transport

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
- [Decision Outcome](#decision-outcome)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
  - [Implementation note (P2, 2026-07)](#implementation-note-p2-2026-07)
  - [Claim-set evolution (P2 1.11, 2026-07)](#claim-set-evolution-p2-111-2026-07)
- [Token Opacity Contract](#token-opacity-contract)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [Encoding: opaque token (chosen) vs discrete fields (rejected)](#encoding-opaque-token-chosen-vs-discrete-fields-rejected)
  - [Transport: query and header (both adopted)](#transport-query-and-header-both-adopted)
- [More Information](#more-information)
- [Option Comparison](#option-comparison)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-file-storage-adr-signed-url-transport`

## Context and Problem Statement

FileStorage authorizes every content operation with a control-minted credential verified by the sidecar
(`cpt-cf-file-storage-adr-sidecar-data-plane`, `cpt-cf-file-storage-design-signed-urls`). That credential is a set of
**claims** — operation, resource (`file_id`, `content_id`/`version_id`), `exp`, constraints (`ip`, token-claim
predicates, upload size/hash, P2 rate/conns), baked response headers — plus a **signature** over them.

Two decisions, previously conflated and re-litigated during review:

1. **Encoding** — carry the claims as **discrete named fields** (one signature over the canonical string, S3 SigV4
   style) **or** as a **single opaque token** that bundles claims + signature into one self-contained string.
2. **Transport / envelope** — **URL query string** **vs.** **HTTP header**.

Earlier drafts chose discrete fields. After team review we **reverse to an opaque token**. The decisive premise:
**FileStorage is deliberately not S3-wire-compatible** — our own host, parameter names, semantics, and crypto (Ed25519,
not HMAC) mean no S3 client, CDN, or tool can ever generate or consume our credential (see *More Information*).
Therefore the *only* parties that ever need to read the claims are the **control plane** (which mints) and the
**sidecar** (which verifies). With no external reader to serve, discrete-field readability is not an asset — it is a
liability that would couple intermediaries to a layout we want free to change.

## Decision Drivers

* **No S3 / external compatibility** — nothing outside control+sidecar parses the credential, so on-the-wire field
  readability buys nothing and only risks coupling
* **Atomicity** — a single self-contained credential is simpler to pass, store, log-redact, and rotate, is signed and
  verified as **one unit**, and cannot be partially stripped, reordered, or tampered
* **Format encapsulation & evolvability** — keeping the credential opaque to everyone but control+sidecar lets us change
  the claim-set *and* the signature/encryption scheme over time **without coordinating with browsers, CDNs, proxies, or
  consuming apps**
* **Sidecar cannot mint** — asymmetric signature: control signs with the private key, the sidecar only verifies with the
  public key
* **Two access intents** — an embeddable **bare URL** (browser, `<img>`/`<video>`, `curl`, media `Range`) and a
  **programmatic / batch** path, both must work
* **A safe, standard token** — avoid JWT's algorithm-agility footguns (`alg` confusion / downgrade)

## Considered Options

* **Encoding:** opaque token **vs.** discrete fields
* **Token standard (if token):** **PASETO `v4.public`** vs. JWT vs. a bespoke format
* **Transport:** query **vs.** header **vs.** both

## Decision Outcome

* **Encoding = one opaque, atomically-signed token.**
* **Token format = PASETO `v4.public`** (Ed25519, asymmetric; **not** JWT). It carries the full claim-set — `op`,
  resource (`file_id`, `content_id`/`version_id`), `exp` (required, capped at `max_url_ttl`, recommended 7 days), the
  constraints (`ip`, token-claim predicates, upload `max_size`/`exact_size`/`expected_hash`; P2 `max_rate`/`max_conns`),
  and the baked response-header set — with **one signature over the whole set**. The PASETO **footer** carries a key id
  (`kid`) for P2 rotation. **Implemented as a codec-equivalent bespoke format, not literal PASETO** — no footer, no
  `kid` — see [Implementation note](#implementation-note-p2-2026-07) below.
* **Transport = both query and header**, chosen by access intent; the **token bytes are identical** either way:
  * **query** — `?fs-token=<token>` — for bare, embeddable URLs (browser, `<img>`/`<video>`, `curl`, media `Range`; the caller
    cannot set headers). Because a query token can leak via server/proxy logs, browser history, and the `Referer`
    header, and P1 has **no per-token revocation**, query-token URLs **MUST** be issued with a **very short TTL**
    (the short `default_url_ttl`, minutes — see DESIGN §4.5); they are for immediate fetch/embed, not durable links.
    Long-lived, shareable, or anonymous links are explicitly **out of P1** and belong to the future **FileShare/P3**
    surface, which will own its own revocation, expiry, and download-count semantics;
  * **header** — `X-FS-Token: <token>` — for programmatic / SDK / batch callers: keeps the credential out of the URL
    (clean logs / no `Referer` leak) and the **URL stable** across re-issue (clean CDN cache). (The token is **never**
    carried in `Authorization` — that header always carries the standard platform JWT.)
  * the query parameter is named **`fs-token`** and the header **`X-FS-Token`**.
* **Why a token, not discrete fields:** because we are not S3-compatible, the discrete-field benefits (external
  readability, edge/CDN/WAF/tooling interop, S3-shape familiarity) are moot — and they would lock intermediaries to our
  field layout. The token is **atomic** (signed/verified/rotated as one unit) and **opaque**, which is what makes the
  format **freely evolvable** (see the Token Opacity Contract).

This supersedes the earlier "discrete fields" outcome. It adopts the PASETO proposal raised in PR review while keeping
the dual-envelope (query + header) and the asymmetric, sidecar-cannot-mint property.

### Consequences

* `cpt-cf-file-storage-design-signed-urls` (DESIGN §4.5), api.md, and the worked examples (§4.6/§4.7) change: the
  discrete `X-FS-*` parameters are replaced by a **single token** carried as `?fs-token=<token>` (query) or an
  `X-FS-Token` header. SigV4-style canonical-string signing is replaced by **PASETO mint (control) / verify
  (sidecar)**; all claims live inside the token.
* **New dependency:** a PASETO v4 library — control plane signs (`v4.public`), sidecar verifies. Ed25519 keys as before
  (private → control, public → sidecar); `kid` in the footer; rotation is P2.
* **FIPS posture (binding constraint on the dependency, not a deferred fallback).** PASETO `v4.public` uses **Ed25519**,
  which *is* approved under **FIPS 186-5**, but approval requires the signing/verifying primitive to run inside a
  **FIPS-validated cryptographic module** — a generic PASETO/Ed25519 crate is not automatically compliant. The binding
  rule for implementation is therefore about *which crate we pull in*:
  * **MUST NOT** introduce any new crate that hard-wires a **non-FIPS algorithm or a self-contained crypto
    implementation we cannot swap out.** No dependency may bake the signing primitive in such a way that the algorithm
    or its backing module is fixed at the crate boundary.
  * The token signer/verifier **MUST sit behind a thin in-house crypto-provider abstraction** (a `SignatureProvider`
    trait: `sign(claims) -> token` on control, `verify(token) -> claims` on the sidecar). The PASETO codec calls that
    abstraction; the abstraction is what binds to the concrete algorithm + module.
  * In FIPS deployments the provider **MUST** be backed by a FIPS-validated module (the platform already ships
    `rustls-corecrypto-provider`); the PASETO/Ed25519 path must route through it. If no validated Ed25519 module is
    available for a target, the provider is swapped for a FIPS-approved alternative (e.g. ECDSA P-256 / a JWS profile
    over the validated module) — **without touching the token codec, claim-set, or the rest of this design**, because
    the token is **opaque and the codec is freely evolvable** (Token Opacity Contract).
  * Concretely this means we evaluate the candidate PASETO crate for this property **before** adding it: prefer one that
    accepts an external signer/key backend (so the algorithm is replaceable), and reject any that statically links a
    non-replaceable non-FIPS implementation.

  This preserves the control-signs / sidecar-verifies / sidecar-cannot-mint property regardless of which module backs
  the provider. The concrete crate + provider choice (and the non-FIPS default) is settled at implementation time, but
  the **replaceability requirement above is not deferrable** — it gates dependency selection.
* **Observability/debuggability** is **sanitized server-side structured logging** by control/sidecar (tenant/file ids,
  outcome; never the token or raw claims). It is **not** done by decoding the token at the edge.
* **Embeddable vs. leak vs. cache:** query envelope (token in URL, short `exp`) for embeddable; header envelope (token
  out of URL, stable cacheable URL) for programmatic.
* Resolves the `CHANGES_REQUESTED` review (adopts PASETO); the discrete-field debate and its pros/cons are removed.

### Confirmation

* Code review confirming the control plane mints the signed token (Ed25519, the bespoke codec-equivalent-to-PASETO
  format described in the Implementation note below) and the sidecar verifies it with the public key, and that
  **no component other than control and sidecar parses the token**.
* Integration tests: the token authorizes via **query** (bare URL, `Range` works) and via **header** (no signing
  material in the URL); a deliberate claim-set / format-version bump verifies end-to-end **without changing any
  intermediary** (browser/CDN/proxy/SDK pass it through unchanged).

### Implementation note (P2, 2026-07)

**Implemented as** a bespoke, codec-equivalent format rather than literal PASETO `v4.public`
(`src/infra/signed_url/mod.rs:9-12`): `base64url(json(claims)).base64url(ed25519_signature)` — the JSON claim-set and
an Ed25519 signature over its serialized bytes, each base64url-encoded and joined with a `.`. There is **no `kid`
field** anywhere in the token (no footer, no key-id claim); the sidecar is configured with a single static public key
and cannot select among multiple keys.

This is an accepted interim measure, not a silent deviation: every property this ADR actually cares about holds —
the token is opaque per the Token Opacity Contract below (only control and sidecar parse it), signed with Ed25519
through the in-house `SignatureProvider`/`SignatureVerifier` abstraction (`Ed25519Provider`) exactly as the FIPS
posture above requires (not a hard-wired crate call, so it satisfies the *replaceability* requirement), and the
control plane remains the sole minter with the sidecar verify-only. What differs is only the concrete codec (bespoke
vs. the PASETO `v4.public` wire format) and the absence of `kid`-based key rotation — and the FIPS posture is
otherwise unchanged: Ed25519 approval under FIPS 186-5 still requires the signing/verifying primitive to run inside a
**FIPS-validated cryptographic module**, and the current `Ed25519Provider` is a generic (non-validated)
implementation, so this codec **MUST NOT** be used in any FIPS-constrained deployment until the provider behind it is
swapped for a FIPS-validated module (or a FIPS-approved alternative such as ECDSA P-256 over a validated module) —
which, because the codec is opaque and evolvable, requires no change to the token format or claim-set, only to the
provider implementation. Migrating the codec to a literal PASETO `v4.public` library, and adding a `kid`/key-rotation
story, remains a tracked P3/Tier-4 follow-up with no dedicated ticket in this repo yet.

### Claim-set evolution (P2 1.11, 2026-07)

The `Claims` struct (`src/infra/signed_url/mod.rs`) gained two fields since the implementation note above: `content_type`
and `etag`, both `String`, `#[serde(default, skip_serializing_if = "String::is_empty")]`. They are populated only on a
download (`op = get`) token — the control plane stamps the version's stored MIME type and its content ETag
(`domain::etag::content_etag`) into the claims at `download-url` issuance time — so that the sidecar, which has no DB
access, can emit real `Content-Type`/`ETag` response headers instead of a generic `application/octet-stream` fallback
with no `ETag` at all. Upload (`op = put`) and multipart-part (`op = multipart_part`) tokens never populate them
(always the empty string, which the `skip_serializing_if` keeps out of the serialized payload).

This is exactly the kind of claim-set change the Token Opacity Contract below anticipates: it required coordinated
changes only to the minter (control plane) and verifier (sidecar), no change to any intermediary, and it is
version-skew-tolerant in both directions by construction — `#[serde(default)]` means a sidecar running this code
verifies a token minted before this change (falls back exactly as it did before), and a sidecar running the prior
code simply ignores the two new fields on a token minted after this change (same as the pre-existing `request_id`
(P2 1.8) and `backend_handle` (P2 1.7) fields, which established this pattern first).

## Token Opacity Contract

This is a hard interface boundary, not a nicety:

* The token's internal format — its **claim-set, encoding, and signature/encryption scheme** — is known **only to the
  control plane (minter) and the sidecar (verifier)**.
* **Every other participant** that sees or forwards the token — browser, CDN, reverse proxy, API gateway, the consuming
  app/LMS, logging/telemetry, the SDK transport layer — **MUST treat it as an opaque, custom byte string**: forward it
  verbatim, and **never parse, base64-decode, inspect, cache-key on, or depend on any part of it**.
* The format **can and will change** — fields may be added, removed, or renamed, and the signature/encryption method may
  be swapped — coordinated **only** between control and sidecar (which deploy together). Anything that parsed the token
  would break on such a change; **opacity is precisely the contract that lets the format evolve without a cross-system
  migration**.
* Therefore: **do not** build CDN/WAF/router/log rules on token internals. Any needed observability comes from
  control/sidecar emitting sanitized structured logs (they know the format), never from decoding the token elsewhere.
* Note on secrecy: PASETO `v4.public` is **signed, not encrypted**, so the payload is technically base64-decodable. That
  does **not** weaken this contract — opacity here is an **encapsulation / evolvability boundary**, enforced by
  convention and by a deliberately-changing format, not a secrecy guarantee. (Field values such as `exp` were never
  secrets anyway; the only secret is the signing key, held solely by control.)

## Pros and Cons of the Options

### Encoding: opaque token (chosen) vs discrete fields (rejected)

**Opaque token (PASETO v4.public) — chosen:**

* Good, because it is **atomic** — one signed unit, impossible to partially strip/reorder/tamper, trivial to pass and rotate
* Good, because the format is **private to control+sidecar and therefore freely evolvable** — claim-set and crypto can
  change with zero coordination with intermediaries (the core driver)
* Good, because no intermediary couples to our field layout; the credential is just bytes everywhere else
* Good, because PASETO `v4.public` is a **safe, fixed-crypto** standard (Ed25519, no `alg` agility / JWT confusion),
  asymmetric so the sidecar cannot mint
* Bad, because it is not human-readable on the wire — **by design**; debugging is server-side, not by eyeballing a URL
* Bad, because the edge cannot read claims — **by design**; this is the encapsulation we want
* Bad, because it adds a PASETO v4 dependency on control and sidecar
* Bad, because `v4.public` is signed-not-encrypted (payload decodable) — addressed by the Opacity Contract, not relied on for secrecy

**Discrete fields — rejected:**

* Their only real advantages are external readability, edge/CDN/WAF/tooling interop, and S3-shape familiarity — **all
  moot** because we are not S3-wire-compatible and explicitly **do not want** intermediaries reading or depending on our
  fields
* They cannot evolve the format freely: adding/renaming a field or changing crypto is an externally-visible wire change
* (They do remain trivially edge-observable — but we are replacing that with sanitized server-side logging on purpose)

### Transport: query and header (both adopted)

* **Query (`?fs-token=<token>`)** — Good: a bare, shareable URL usable in a browser/`<img>`/`curl`/media `Range` with no
  headers. Bad: the token sits in the URL (logs/`Referer`/history — mitigated by short, capped `exp`) and the URL changes
  on re-issue (CDN cache-key churn).
* **Header (`X-FS-Token`)** — Good: token out of the URL (clean logs), stable URL across re-issue
  (clean cache), tidy for batch/SDK. Bad: not a bare URL — the caller must set headers, so it cannot be embedded.

Both are adopted; the caller (or SDK) picks by intent — query for embedding, header for programmatic.

## More Information

**PASETO `v4.public`** is a self-contained token: `v4.public.<base64url(payload)>.<base64url(footer)>` where the payload
is the claim-set and the signature is Ed25519 over the canonical PASETO pre-auth encoding; verification uses the public
key only. Unlike JWT it has **no algorithm field** — the version pins exactly one scheme, eliminating `alg`-confusion
and downgrade attacks. We use the footer for the `kid`.

**Why not mimic S3's discrete-field shape:** S3 SigV4 (and its many re-implementations) deliberately uses readable
discrete params because S3 *is* an open, multi-client wire contract. We are the opposite: a closed credential between
two components we control. Our crypto (Ed25519 vs HMAC-SHA256), param semantics, and resource addressing (the URL points
at our sidecar, not a backend) already make S3 compatibility impossible — so we gain nothing by imitating its shape, and
an opaque, evolvable token serves our actual two-party, evolvable contract far better.

## Option Comparison

✓ = yes / good · ✗ = no / bad

| Aspect | Token + query (chosen) | Token + header (chosen) | Fields + query | Fields + header |
|---|---|---|---|---|
| Bare, shareable URL (no headers) | ✓ | ✗ | ✓ | ✗ |
| Credential kept out of the URL | ✗ | ✓ | ✗ | ✓ |
| Stable URL across re-issue (cache) | ✗ | ✓ | ✗ | ✓ |
| Format evolvable without external coupling | ✓ | ✓ | ✗ | ✗ |
| Atomic credential (one signed unit) | ✓ | ✓ | ✗ | ✗ |
| One signature | ✓ | ✓ | ✓ | ✓ |
| **Verdict** | **Chosen (embeddable)** | **Chosen (programmatic)** | Rejected | Rejected |

## Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Related**: [ADR-0003: Split the Data Plane into a Signed-URL Sidecar](./0003-cpt-cf-file-storage-adr-sidecar-data-plane.md)

This decision directly addresses the following requirements or design elements:

* `cpt-cf-file-storage-fr-signed-urls` — the credential is a PASETO `v4.public` token carried in the query or a header
* `cpt-cf-file-storage-design-signed-urls` — claims move inside the token; PASETO mint/verify replaces canonical-string signing
* `cpt-cf-file-storage-principle-signed-urls` — control-minted (private key), sidecar-verified (public key); the sidecar cannot mint
* `cpt-cf-file-storage-nfr-bandwidth` — the header envelope gives programmatic callers a stable, cache-friendly URL
