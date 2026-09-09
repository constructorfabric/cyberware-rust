---
status: proposed
date: 2026-09-08
---

Created:  2026-09-08 by Constructor Tech
Updated:  2026-09-09 by Constructor Tech

# ADR-0004: Credential Record and Secret Value as Separate Resources

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
  - [Axis A — resource modelling](#axis-a--resource-modelling)
  - [Axis B — how a value is asked for](#axis-b--how-a-value-is-asked-for)
- [Decision Outcome](#decision-outcome)
  - [Resulting surface](#resulting-surface)
  - [Why `credentials` and not `secrets`](#why-credentials-and-not-secrets)
  - [Why the path key stays a reference, not a UUID](#why-the-path-key-stays-a-reference-not-a-uuid)
  - [One write verb per resource: no `PATCH`, no `POST`](#one-write-verb-per-resource-no-patch-no-post)
  - [Consequence: no single-request create of record and value](#consequence-no-single-request-create-of-record-and-value)
  - [Bulk secret read: two selectors, no pagination](#bulk-secret-read-two-selectors-no-pagination)
  - [Consequences](#consequences)
  - [Confirmation](#confirmation)
- [Backward Compatibility by Mode](#backward-compatibility-by-mode)
- [Impact Analysis by Domain](#impact-analysis-by-domain)
  - [Security](#security)
  - [Authentication and authorization](#authentication-and-authorization)
  - [Integration and contract](#integration-and-contract)
  - [Operations](#operations)
  - [Performance](#performance)
  - [Reliability](#reliability)
  - [Data](#data)
  - [Maintainability and evolution](#maintainability-and-evolution)
  - [Compliance](#compliance)
  - [Not applicable](#not-applicable)
- [Revisit Triggers](#revisit-triggers)
- [Pros and Cons of the Options](#pros-and-cons-of-the-options)
  - [A1: `secrets` collection, value on the item, metadata at `meta`](#a1-secrets-collection-value-on-the-item-metadata-at-meta)
  - [A2: `credentials` collection, metadata on the item, value at `secret` (CHOSEN)](#a2-credentials-collection-metadata-on-the-item-value-at-secret-chosen)
  - [A3: `secrets` collection, value at `value`](#a3-secrets-collection-value-at-value)
  - [B1: value only at its own address (CHOSEN)](#b1-value-only-at-its-own-address-chosen)
  - [B2: one address, value opted in by query parameter](#b2-one-address-value-opted-in-by-query-parameter)
  - [B3: one address, value opted in by request header](#b3-one-address-value-opted-in-by-request-header)
  - [B4: response shape decided purely by permissions](#b4-response-shape-decided-purely-by-permissions)
  - [B5: values on the paginated metadata collection](#b5-values-on-the-paginated-metadata-collection)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-secret-value-exposure`

## Context and Problem Statement

The gear ships one read surface: `GET /credstore/v1/secrets/{ref}` returns the secret value together with its access metadata, and there is no collection read at all (the PRD lists secret listing as a non-goal; DESIGN §4.4 states "there is no LIST"). One PDP action, `read`, covers all of it.

Product requirements now introduce readers who must **not** see values, and readers who need several values at once:

- **Integration administrator.** Configures SMTP, provider keys and webhooks for a tenant. Needs the catalogue, the inheritance status of every entry, and the ability to rotate, retarget and reset values. Does not need — and should not have — the plaintext of the credential being rotated.
- **Catalogue and audit view.** "What is configured here, since when, does it expire, is it ours or inherited", with no value anywhere in the response.
- **Runtime application, one credential.** Reads the value of a known name in the tenant it acts for, restricted to the credentials that belong to it.
- **Runtime application, its whole credential set.** A common pattern: a mail service wants *all* credentials that belong to it — "all SMTP secrets", expressed as a selector over category or type — in one round-trip, at startup or per request, rather than N sequential reads.

Both needs are already recorded as open questions in this gear's own documents rather than invented here: `PRD.md:717` and `DESIGN.md:790` note a future `POST /secrets/batch` for multi-credential retrieval, and `PRD.md:720` / `DESIGN.md:793` note a P2 metadata list endpoint that "must be reconciled with anti-enumeration" and needs a dedicated design pass. This ADR answers the value-exposure half of both.

Two questions must be answered together, because the answer to one constrains the other:

1. **Resource modelling.** Today the entity URL returns the sensitive payload, and the safe view would have to be a suffix on it. Is the addressable thing the *credential record* (with the secret as a sub-resource), or the *secret* (with metadata as a suffix)?
2. **Value exposure.** By which mechanism does a caller ask for a value — a distinct address, a query flag, a header, or implicitly through its permissions — and how is the multi-credential case served without turning any endpoint into a bulk-disclosure primitive?

## Decision Drivers

- **D1 — correctness over continuity.** The target shape is chosen on its merits. Where it breaks something that ships today, the break is stated per surface in [Backward Compatibility by Mode](#backward-compatibility-by-mode) and paid for during rollout; it is not designed around. Continuity is an output of this ADR, never an input.
- **D2 — three distinct privileges.** Enumerating entries, reading one record's metadata, and reading a secret value have different blast radius and must be separately grantable.
- **D3 — a refusal is indistinguishable from absence.** Reads outside the caller's grants surface as the canonical 404, including per item inside a bulk response (`nfr-tenant-isolation`).
- **D4 — a value-blind writer must still get a CAS validator.** Writes require `If-Match` (`fr-optimistic-concurrency`) and the strong validator is an HTTP header, so some response a metadata-only role may call has to carry the `ETag`.
- **D5 — value disclosure must be observable and throttleable by route.** The gateway matches route policies on **paths**, and audit should record "this subject read this value" from the operation, not by inspecting bodies.
- **D6 — bulk reads may never exceed the caller's own read scope, and may never be walked.** A bulk value read is acceptable when the selector can only ever match what the caller is already entitled to read; it is unacceptable when it can be paginated through the catalogue, or when it silently truncates.
- **D7 — bounded authorization cost on the hot path.** Applications read values continuously; the decision must not add PDP evaluations per request just to decide what to include.
- **D8 — one shape per address.** A response schema must not depend on who asked, so that generated specs and typed SDKs carry a real guarantee.

## Considered Options

### Axis A — resource modelling

- **A1** — `secrets` collection; the value lives on the item (`/secrets/{ref}`), metadata is a suffix (`/secrets/{ref}/meta`).
- **A2** — `credentials` collection; metadata lives on the item (`/credentials/{ref}`), the value is a sub-resource (`/credentials/{ref}/secret`).
- **A3** — `secrets` collection; metadata on the item, value at `/secrets/{ref}/value`.

### Axis B — how a value is asked for

- **B1** — only at its own address (the sub-resource, plus a dedicated bulk address).
- **B2** — one address, value opted in by query parameter (`?show-secret=true`).
- **B3** — one address, value opted in by request header.
- **B4** — one address, the `value` field present if and only if permissions allow.
- **B5** — values carried by the paginated metadata collection (`?show-secrets=true`).

## Decision Outcome

**Chosen: A2 + B1.** The addressable entity is the **credential record**, which never carries the secret; the **secret value is a sub-resource** with its own address, and the multi-credential case is served by one dedicated bulk address with a capped, scope-bounded selector.

The deciding arguments are D8 (metadata surfaces cannot carry a value because the value is not part of that resource — a URL fact rather than a test invariant), D5 (every exposure class is its own path, so gateway policy, rate limits and audit selectors are expressible), D2 and D6.

### Resulting surface

| Address | Returns | PDP action | Notes |
|---|---|---|---|
| `GET /credstore/v1/credentials` | credential records (metadata), paginated | `list_meta` | value-free by construction; `Cache-Control: no-store`, because the body varies by tenant and subject; OData filter/order per `guidelines/DNA/REST/PAGINATION.md` |
| `GET /credstore/v1/credentials/{ref}` | one credential record (metadata) | `read_meta` | carries the `ETag` — the CAS validator source (D4); `ETag` only for the tenant's own row; `Cache-Control: no-store` |
| `PUT /credstore/v1/credentials/{ref}` | — (201 on create with `ETag`, 204 on replace) | `write_meta` | create-or-replace of the **record only**: sharing, type, category, expiry. Preconditions carry the intent: `If-None-Match: *` create-only, `If-Match: "<etag>"` guarded replace, `If-Match: *` last-writer-wins |
| `DELETE /credstore/v1/credentials/{ref}` | — (204) | `delete` | `If-Match` required; releases the reference |
| `GET /credstore/v1/credentials/{ref}/secret` | the value + the record | `read_value` | `Cache-Control: no-store`; audited |
| `PUT /credstore/v1/credentials/{ref}/secret` | — (204) | `write_value` | set or rotate; `If-Match` required and evaluated against the **record's** validator (see below); never creates a record |
| `POST /credstore/v1/credentials:read-secrets` | values for a bounded selection | `read_value`, per item | bulk; see below |

Suppression (the "disabled here" tombstone, if adopted) attaches to the record: `POST` / `DELETE /credstore/v1/credentials/{ref}/suppression`.

### Why `credentials` and not `secrets`

Under A2 the collection name has to describe the record, not the payload. "Credential" is the record — a named, typed, tenant-scoped entry with sharing, version and expiry; "secret" is the sensitive value it holds. That vocabulary is the one this project already uses when talking about the two read classes, and it removes the sentence A1 forces you to write: *"`GET /secrets` returns no secrets."*

It also converges with the platform: the Constructor service that already implements a version of this domain is `credentials-storage`, whose collections are `credential-definitions` and `credentials`. If the gear becomes its successor, a shared noun makes the migration and any compatibility facade legible instead of a translation exercise.

### Why the path key stays a reference, not a UUID

`{ref}` remains the caller-chosen `SecretRef` (`[A-Za-z0-9_-]+`), not the row UUID. The reference is the stable name a consumer hard-codes; the row id is a *generation* id, minted fresh whenever a credential is deleted and recreated, and it is deliberately kept out of response bodies so that CAS validators have exactly one source (the `ETag`). A UUID in the path would force consumers to resolve name → id on every call and would make a recreated credential a different URL, breaking the one property applications rely on.

### One write verb per resource: no `PATCH`, no `POST`

Each resource has exactly one write verb, `PUT`, and the intent of a write is carried by RFC 7232 preconditions rather than by the verb:

| Intent | Request |
|---|---|
| create the record, fail if it exists | `PUT /credentials/{ref}` + `If-None-Match: *` → 201, or 412 if present |
| replace the record, only if unchanged since I read it | `PUT /credentials/{ref}` + `If-Match: "<id>.<version>"` → 204, or 409 |
| replace the record, last writer wins | `PUT /credentials/{ref}` + `If-Match: *` → 204 |
| set or rotate the value | `PUT /credentials/{ref}/secret` + one of the same three preconditions |

`PATCH` is deliberately absent. A partial-update verb needs its own merge semantics and its own conflict rules on top of the ones `PUT` already has, and it invites the pattern where a client mutates one field without ever holding the whole record — which is precisely how a `sharing` change gets made without noticing the expiry it left behind. `PUT` on the record is a whole-value replace: fields absent from the body are reset to their defaults, exactly as today's value `PUT` already clears an omitted expiry.

`POST` on the collection is absent too, because `PUT` with `If-None-Match: *` already expresses create-only, keeps the write idempotent, and puts the name in the URL where it belongs rather than in the body. The one thing `POST` bought — creating the record and its value in a single atomic request — is given up on purpose; see the next section.

The record's `type` remains immutable: a `PUT` whose body names a different type is rejected rather than applied, so "whole-value replace" covers the mutable metadata only.

**Versioning across the two resources.** One monotonic `version` lives on the record, and both writes bump it. A validator read from the record therefore guards value writes as well, and a concurrent rotation invalidates a pending metadata edit. That is conservative — a metadata edit can be rejected because someone rotated the value — but it keeps one validator, one counter and one `ETag` in the system. Independent per-sub-resource `ETag`s would be more precise and are the obvious extension if the false conflicts ever hurt.

### Consequence: no single-request create of record and value

**A new credential now takes two requests**, and this is an accepted cost of the resource split:

```
PUT /credstore/v1/credentials/smtp-default        If-None-Match: *   → 201
PUT /credstore/v1/credentials/smtp-default/secret If-Match: *        → 204
```

**What a value-write precondition is compared against.** The value sub-resource has no validator of its own, because a record and its value share one `version` — a value write bumps it exactly as a record write does. A precondition on `PUT …/{ref}/secret` is therefore evaluated against the **record's** validator, the same `"<id>.<version>"` the record's `ETag` carries.

This is a deliberate deviation from RFC 9110, which evaluates a precondition against the target resource's own current representation, and it is stated rather than left to be inferred: read the other way, `If-Match` on a value that has never been written would be evaluated against an absent representation and could not succeed, which would make the first of the two writes above unexpressible. The record's `ETag` exists from the moment the record is created — which is why record creation returns it, rather than making the client insert a metadata `GET` between the two calls — so a guarded first value write is available immediately, and `If-Match: *` on the sub-resource means "the record must exist". A value write against a reference with no record is a 404, not a create: the record is the thing that gets created, and only by its own `PUT`.

Between them the record exists **without a value**, so that state has to be legal and defined rather than transient:

- **A value-less record does not resolve and does not shadow.** "Does not resolve" means it is not a candidate, not that the read returns 404: the walk continues past it, so a `GET .../secret` returns an ancestor's `shared` value when the chain offers one and the canonical 404 only when nothing in the chain does. That is the whole point — critically, the record does **not** hide an inherited value from an ancestor. A half-finished create in a child tenant must not silently break inheritance that was working before it started. This is the one rule with a claim on a *second* surface: the collection read reduces a reference's rows to the one a value read would resolve, so its reduction has to skip a `declared` local row in favour of a resolvable inherited one, or the listing would report the empty local record while the point read serves the ancestor's value ([ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md) "Reducing a reference to one item").
- **It is a distinct state, not "unfenced", and it is stored.** Today a `NULL` value fingerprint means "seeded out of band, serve on trust". A declared-but-empty record has no value at all, so the two cases must be told apart explicitly — by a lifecycle state on the record, not by the nullability of the fingerprint. Concretely: a fourth `status`, `declared`, widening the `CHECK (status IN (1, 2, 3))` the initial schema carries. Naming the state without giving it a column would leave every predicate that has to distinguish it — resolution, the reaper sweep, the collection read — resting on application logic over a value the metadata row does not hold; DESIGN §6.1 sets those predicates out. It is deliberately *not* a new nullable marker: `status` is already the lifecycle column, and the sweep and resolution filters that must exclude the state are already written against it, so they need no change at all.
- **The reaper must not sweep it.** With `declared` as its own status this falls out rather than being enforced: the sweep selects `provisioning` rows past their timeout, and a `declared` record is not one. Had the state been folded into `provisioning`, the sweep would have needed a timeout exception for a record whose value write may legitimately arrive days later — or never — and a slow operator would lose their record between the two calls.
- **Atomicity moves to the client.** The gear no longer guarantees "either both parts exist or neither"; a client that abandons the sequence leaves an empty record behind. The value write remains crash-safe on its own (backend write plus fingerprint stamp in one saga), so what is lost is only the coupling between the two requests.
- **The UI consequence is real**: an administrator who creates a credential in a console sees a two-step flow, and the console must either drive both calls or show the record as incomplete.

If atomic creation is later required — for instance because empty records start appearing in production catalogues — it returns as a collection-level `POST` carrying both parts, and this ADR is amended rather than reinterpreted (see [Revisit Triggers](#revisit-triggers)).

### Bulk secret read: two selectors, no pagination

`POST /credstore/v1/credentials:read-secrets` accepts exactly one of two selectors. The filtered form uses the **platform OData syntax**, declared through `OperationBuilderODataExt::with_odata_filter`, not a bespoke JSON object — a hand-rolled `$`-parameter would also trip the `de0802_use_odata_ext` lint:

```jsonc
// explicit — the caller knows the names (request body)
{ "references": ["smtp-default", "stripe-key", "webhook-signing"] }
```

```http
// scoped filter — "all my SMTP credentials" (query parameter, empty body)
POST /credstore/v1/credentials:read-secrets?$filter=category eq 'email-sender'
```

**There is a precedent for exactly this shape.** `POST /usage-collector/v1/records/aggregate` accepts `$filter` through the same ext trait, deliberately declares no `limit`, `cursor`, `$orderby` or `$top`, and returns a domain result rather than a `Page<T>`; its handler documents that omission explicitly. Crucially, it replaces the cursor with a *different* cost bound: a mandatory time window, rejected with `MISSING_TIME_WINDOW` when absent. Our analogue of that bound is cardinality — the cap below. It is the only such route in the platform today, so the pattern is legal but load-bearing on that substitution, and the ADR states it rather than leaving it implicit.

Rules, all of which follow from D6:

- **A selector narrows; resolution decides.** Both the caller's `$filter` and the policy's own `category` clamp are pushed into SQL, but only as a way to pick candidate *references* cheaply. Each candidate is then resolved over **all** its rows across the chain, exactly as a point read would, and the winner is authorized. Applying either predicate below the resolution would change which row wins and could report an ancestor's credential as the effective one where the point read refuses it; [ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md) "How authorization applies to a collection" sets out that failure and the invariance rule that bounds it. Only `category`, `type` and `reference` are eligible for the SQL step, because only those are invariant across a chain by requirement.
- **The selector cannot widen scope.** Each match resolves through the ancestor chain exactly as a point read does, and every item is authorized as `read_value` on its own resolved type and category. A filter therefore returns a subset of what N point reads would return; it saves round-trips, never privileges. For an application whose grant *is* "values of category `email-sender`", "all my SMTP secrets" discloses nothing it could not already fetch by name.
- **The selector vocabulary is deliberately small** and every field in it must be backed by an index: `references` in the body, or `$filter` over `category`, `type` and `reference` with `eq` / `in`. No `owner_tenant_id`, no ordering, no `$select`. Today `credstore_secrets` indexes none of `category`, `secret_type_uuid` or `sharing` individually, so shipping the filtered selector requires the migration that adds them; without it the filter is a sequential scan, and the platform checks the "indexed fields only" rule by review, not automatically.
- **No pagination and no cursor.** Values must not be walkable; a paginated value read is a catalogue dump in slow motion.
- **Hard cap with an explicit failure, never truncation.** Above the cap (proposed: 25) the request fails with `400 TOO_MANY_MATCHES` and the client is expected to narrow the selector. Truncation would both hide credentials from a legitimate caller and turn the endpoint into a drip.
- **The response is not a `Page<T>`.** No `items`/`page_info` envelope, no `next_cursor` — the type is a flat per-reference result list, so nothing in the contract suggests the result can be continued.
- **Per-item outcome under HTTP 200.** Not-resolved, out-of-scope, suppressed and fence-mismatch all report the same "not found" outcome per item (D3), and a failing item never aborts the batch.
- **The two selectors differ in what a refused item may say.** With the **explicit** selector the response carries one item per reference the caller supplied, "not found" included: echoing a name back to whoever just sent it discloses nothing. With the **filtered** selector a refused row must be **omitted entirely** rather than reported as a "not found" item, because its `reference` was never in the caller's hands — the filter found it, and naming it would turn a value endpoint into an oracle for names the caller may not read. The consequence is that the filtered form's item count is not a fixed function of the request, which is the same property the collection read already has (ADR-0005 "Pagination over a reduced result") and the reason neither surface reports a total.
- **Per-item fence.** Every returned value is verified against its row fingerprint (ADR-0003).
- **`Cache-Control: no-store`**, one audit record per value actually returned, and its own gateway rate-limit rule on its own path.
- **The cap is enforced by fetching `cap + 1`, never by counting.** No `COUNT` query is issued: if a `cap + 1`-th row comes back, the request fails; otherwise the rows fetched are the answer. This matches the platform rule against counting queries and keeps the check one indexed read.

One response shape serves both selectors — a per-item outcome, and the record alongside each value so the caller can see whether it got its own or an inherited credential:

```jsonc
POST /credstore/v1/credentials:read-secrets?$filter=category eq 'email-sender'
// empty body — the filtered selector travels in the query, per the two forms above

200 OK
Cache-Control: no-store
{
  "items": [
    {
      "reference": "smtp-default",
      "outcome": "ok",
      "secret": "…",
      "credential": {
        "owner_tenant_id": "…partner-a…",
        "sharing": "shared",
        "is_inherited": true,
        "inheritance": "inherited",
        "version": 3,
        "type": "gts.cf.core.credstore.secret.v1~cf.core.credstore.basic_auth.v1~",
        "category": "email-sender",
        "expires_at": null
      }
    },
    // No `not_found` entry appears under a filtered selector: a reference the
    // filter found but the caller may not read is omitted entirely, because
    // its name was never in the caller's hands. Under the *explicit*
    // selector the same refusal does appear, as
    // `{ "reference": "…", "outcome": "not_found" }`, because echoing back a
    // name the caller just sent discloses nothing.
  ],
  "returned": 2,
  "cap": 25
}
```

`outcome` is `ok` or `not_found`; the latter covers "does not resolve", "out of scope", "suppressed" and "fence mismatch" without distinguishing them (D3). `returned` is the length of `items`, not a database count. With the explicit-`references` selector the response lists one item per requested reference, in request order, so a client can zip its request to the result; with `select` the order is `reference` ascending.

### Consequences

- Metadata can never carry a value, because the value is not part of the metadata resource. The invariant is structural, so it cannot be reintroduced by an incautious DTO change.
- Every exposure class is a distinct path, so route policies, rate limits and audit selectors are expressible without body inspection (D5). What that does **not** buy is a single glob: `/credentials/*/secret` matches the value sub-resource but not `POST /credentials:read-secrets`, which discloses values from a path with no `secret` segment. Gateway rate-limit and audit rules must therefore name both addresses explicitly; a rule written as one glob silently leaves the bulk route unthrottled and unaudited, which is the more dangerous of the two.
- A value-blind administrator rotates by reading the record for its `ETag` and writing the sub-resource; it never touches an address that returns a secret (D4).
- The frequent "give me my whole credential set" pattern is served in one call, with disclosure bounded by the caller's own grant, a cap, and the absence of pagination (D6).
- The list item and the point record share one schema, which is what clients expect and what generated SDKs can type.
- Cost: this renames the shipped collection and moves the value read to a sub-resource. Every current consumer of the value changes. That is accepted under D1 and recorded below.
- Cost: one more endpoint than a flag-based design, and a `POST` used for a read. The latter has precedent in this codebase (`POST /usage-collector/v1/records/aggregate`) and has a side benefit for secrets: a `POST` response is not cached by intermediaries.
- Cost: the bulk endpoint performs up to the cap backend reads and fence verifications per request, plus one PDP evaluation per distinct type; it needs its own latency budget.
- Cost: bulk clients handle partial results, which is more code than N point reads that either succeed or 404.
- Cost: creating a credential is two requests with no atomicity between them, and "record without a value" becomes a legal state the domain, the reaper and the resolution rules all have to account for.
- Benefit of the same trade: metadata can be edited without touching the value at all, which is what makes a value-blind administrator role possible in the first place; with only a value-bearing `PUT` that role could not change a sharing mode without rewriting the secret.

### Confirmation

- Contract tests: no response schema under `/credentials` or `/credentials/{ref}` contains a `value` property; `value_fp` and `fp_key_id` appear in no schema at all.
- E2E: a role with `read_meta` but not `read_value` reads the record, obtains the `ETag`, completes a guarded `PUT …/secret`, and still receives 404 on `GET …/secret` — byte-identical to the 404 for a name that does not exist.
- E2E: the bulk endpoint rejects a selector matching more than the cap with `TOO_MANY_MATCHES`; an item the caller may not read is reported identically to an item that does not exist; a fence-poisoned item does not affect its siblings.
- E2E: an application granted `read_value` on one category receives exactly its own credentials for a `$filter` on that category, and an empty result for another category.
- E2E: the bulk endpoint accepts `$filter` but rejects `cursor`, `$orderby`, `$top` and `limit`; its response carries no `page_info`.
- E2E: `PUT /credentials/{ref}` with `If-None-Match: *` returns 201 the first time and 412 the second; a `PUT` naming a different type is rejected; a `PUT` that omits expiry clears it.
- E2E: a record created without a value returns 404 on its secret **and** an ancestor's inherited value keeps resolving for descendants while the record stays empty.
- E2E: the reaper leaves a deliberately empty record untouched across at least two sweep intervals.
- Audit: one record per value returned by both the point read and the bulk read; no record claims a disclosure when the response carried none.

## Backward Compatibility by Mode

Compatibility is recorded here, not optimized for (D1).

| Surface / mode | Compatible with what ships today | Detail |
|---|---|---|
| Value read | **no — moves** | `GET /credstore/v1/secrets/{ref}` becomes `GET /credstore/v1/credentials/{ref}/secret`. Every consumer of a value changes its URL. Same body plus the record, same `ETag` and `no-store`. |
| Entity URL semantics | **no — inverts** | The item URL used to return the secret; it now returns the record. A client that keeps calling the old shape gets metadata, never a value, so the failure is a missing field rather than a silent disclosure. |
| Collection name | **no — renamed** | `secrets` → `credentials`. |
| Create | **no — removed** | `POST /credstore/v1/secrets` with a value in the body disappears. Creation becomes `PUT /credentials/{ref}` + `If-None-Match: *` for the record, then `PUT …/secret` for the value: **two requests, no atomicity**. |
| Rotation | **no — moves** | `PUT /credstore/v1/secrets/{ref}` becomes `PUT /credstore/v1/credentials/{ref}/secret`; the `If-Match` contract itself is unchanged. |
| Metadata edit | n/a — new | Previously impossible without rewriting the value; now `PUT /credentials/{ref}`. |
| List, bulk read, suppression | n/a — new | Nothing to break. Listing is a documented non-goal today, so shipping it amends the PRD rather than breaking a contract. |
| SDK trait `CredStoreClientV1` | **mostly** | `get` (value), `put` (rotate) and `delete` keep their names and meaning, re-pointed at the new addresses. `create` can no longer be one call: it either becomes a convenience wrapper that issues both writes and documents its non-atomicity, or it is replaced by `put_record` + `put_secret`. New methods (`metadata`, `list_metadata`, `read_secrets`) ship with default "unsupported" implementations, so existing implementors and test doubles keep compiling. |
| **Authorization model (actions)** | **no — broken on purpose** | `read` splits into `read_value`, `read_meta` and `list_meta`; `write_meta` is added. Every policy granting `read` is re-issued. |

**Why the authorization break is not softened.** Accepting `read` as a synonym on the new metadata surfaces would let deployments roll forward untouched, at the price of a grant whose meaning is permanently ambiguous: a reviewer could not tell whether `read` was meant to include value disclosure. Since the premise of this ADR is that value disclosure is a separate privilege, an ambiguous grant defeats it. The split is clean, and re-granting is a release task with an explicit checklist: enumerate every role holding `read`, decide per role whether it needs values, the catalogue, or both, issue the new grants.

**Rollout ordering that follows.** Grants first, endpoints second. If endpoints ship before grants are re-issued, reads fail closed — empty pages and 404s — which is safe but reads like an outage. The reverse order is safe and invisible. HTTP consumers of the value migrate on their own schedule only if the old path is kept alive as a temporary alias; whether to provide that alias is a rollout decision, and this ADR does not require one.

## Impact Analysis by Domain

### Security

- **Assets and adversary.** The asset is the plaintext credential. The adversary of record is an over-granted or compromised principal inside the platform — a tenant user, an application token, a stolen bearer. Two goals are addressed: learning *which* credentials exist (reconnaissance) and obtaining *many* values in one action (exfiltration).
- **Attack surface delta.** Metadata addresses widen only the reconnaissance surface, deliberately, because the catalogue is a product requirement, and each is gated by its own action. Exactly two addresses can disclose a value — `GET /credentials/{ref}/secret` and `POST /credentials:read-secrets` — and both must be audited and separately throttled. They do **not** share a path prefix: see the note above on why one glob does not cover them.
- **Reconnaissance containment.** Every refusal is the canonical 404, byte-identical to absence, including per item in a bulk response. Timing is not equalized; that is a pre-existing, accepted property of the gear (a resolved credential consults the PDP and the registry, a missing one does not — DESIGN §5.4).
- **Exfiltration containment.** No response can exceed the caller's own read scope, none is paginated, and the bulk endpoint fails rather than truncates above its cap. The worst case for a compromised application token is "the credentials that token was already entitled to read", which is the same bound as with N point reads, reached faster.
- **Per-option risk** is analyzed inline below; the two material risks are values on the paginated collection (unbounded, walkable disclosure) and a header-driven opt-in (invisible to path-based policy and to access logs).
- **Data protection.** Values keep `no-store`, appear in no metadata schema, and are never logged. `value_fp` and `fp_key_id` never leave the gear on any surface.

### Authentication and authorization

- **AuthN:** unchanged. No new principal kinds, no session or token-format change.
- **AuthZ model:** `read` splits into `read_value`, `read_meta`, `list_meta`; `write_meta` is added; `write_value` covers create and rotate; `delete` is unchanged. The bulk endpoint introduces **no new action** — it evaluates `read_value` per item, so it can never grant more than N point reads.
- **Compatibility of the model:** deliberately not preserved; see the table above. No synonym for `read` is accepted.
- **Escalation path to watch.** Whoever may change a credential's `category` changes which application may read its value, and under the scoped selector that also changes which credentials appear in an application's bulk result. That right belongs to `write_meta`, its values come from a registry, and the PDP policy owner must treat it as privileged rather than cosmetic.

### Integration and contract

- **Breaking changes:** yes, and enumerated above. The value read and the collection name move; in-process SDK consumers are shielded because the trait keeps its method names.
- **Contract additions:** the metadata collection, the metadata point read, metadata mutation, the secret sub-resource, the bulk read, and suppression if adopted.
- **Consumer action required:** HTTP consumers of a value update one URL; consumers wanting the new capabilities implement them. Bulk clients must handle per-item outcomes, which is part of that endpoint's published contract.
- **Prerequisites:** amending the PRD non-goal on listing, and the companion ADR for the collection read under the no-projection PEP contract (DESIGN §4.4).
- **Deprecation:** nothing is deprecated. If a temporary alias for the old value URL is provided, its removal date belongs to the rollout plan, not to this ADR.

### Operations

- **Rate limiting.** Gateway route policies match paths, which is why exposure classes are separate paths: value reads get stricter rules than catalogue reads, and the bulk address gets its own.
- **Audit.** One record per value returned, on both value addresses (`GET /credentials/{ref}/secret` and `POST /credentials:read-secrets` — named, not globbed). Metadata reads are not audited per record; the collection read may be sampled if volume warrants.
- **Metrics.** Bulk selector kind and result size distribution, per-item refusal counts by cause, cap rejections. Existing read-outcome and fence metrics extend unchanged.
- **Runbook.** A spike of `TOO_MANY_MATCHES` or of per-item refusals is the operator-visible signal of a misbehaving or probing client.
- **No new infrastructure.**

### Performance

- **Metadata reads** are strictly cheaper than today's read: one indexed resolution, no backend call, no fence verification.
- **Point value read** is unchanged in cost.
- **Bulk** performs up to the cap backend reads and fence verifications, plus one PDP evaluation per distinct type present in the result. It carries its own SLO and is excluded from the point-read latency budget.
- **Why the cap is the control.** Without it, request cost and disclosure blast radius grow together; one number bounds both.

### Reliability

- **Partial failure is normal** in bulk: an item may fail to resolve, be refused, be suppressed, or fail its fingerprint check, and the rest are still served.
- **No new write failure modes.** Reads plus one metadata mutation that never touches the value backend, so the provisioning and deprovisioning sagas are unaffected.
- **Fence behaviour preserved:** a mismatch still fails closed, now scoped to one item.

### Data

- **No schema change originates here.** The metadata fields these addresses expose (sharing, version, expiry, and the proposed `category`) come from the companion work.
- **No new personal data.** References and categories are operator-chosen labels; values stay opaque bytes to the gear.
- **Retention and residency:** unchanged; nothing new is persisted.

### Maintainability and evolution

- A future exposure class is a new address, not a new flag, so every response keeps one shape and every policy stays path-addressable.
- If a legitimate caller outgrows the cap, the answer is to raise it deliberately — a reviewed configuration change with an audit consequence — not to add pagination to a value read.
- "Metadata carries no value" is guaranteed by the resource layout and pinned by contract tests, so it survives refactoring instead of relying on reviewer vigilance.

### Compliance

- Supports `nfr-confidentiality`: values stay out of caches, logs and metadata schemas, and every disclosure is attributable to a subject and an operation.
- Separating "may see that a credential exists" from "may read it" is what makes least privilege expressible for the integration-administrator role, which is the compliance-relevant outcome.

### Not applicable

- **Usability:** the gear exposes no end-user interface; the administrator experience lives in the consuming console, and this ADR only fixes the API it consumes.
- **Session management:** the gear holds no sessions.
- **Data migration:** the decision adds and moves endpoints; no stored data changes.
- **Penetration testing as a gate:** the security confirmation at ADR level is the contract and e2e invariants listed above; scheduled testing is a platform activity outside this decision.

## Revisit Triggers

- A caller needs more credentials per request than the cap can serve, or needs a selector richer than `eq`/`in` over category, type and reference — prefix or range matching on `reference` is the likely first ask, and it is out of the grammar above on purpose, because a prefix scan over names is the enumeration primitive this ADR exists to withhold.
- The gateway gains query-aware or header-aware route policies, which would remove the main argument against a flag-based opt-in (B2, B3).
- Audit starts deriving "value read" from response bodies rather than from the operation, which would remove the second argument.
- The metadata collection acquires a use case that genuinely needs values inline; that would reopen B5 and require a new answer to walkability.
- Empty records start accumulating in production because clients abandon the two-step create; the fix is a collection-level `POST` that carries record and value together, restoring atomicity.
- False conflicts between metadata edits and value rotations become a real annoyance; the fix is an independent `ETag` per sub-resource instead of one shared `version`.

## Pros and Cons of the Options

### A1: `secrets` collection, value on the item, metadata at `meta`

- Good: the value read keeps its current URL, so HTTP consumers do not move.
- Bad: the entity URL returns the sensitive payload and the safe view is the exception, which inverts the default one wants for credentials.
- Bad: the list item and the point item have different schemas — the collection omits a field the item returns — so clients cannot reuse one type (D8).
- Bad: forces the sentence "`GET /secrets` returns no secrets" into the documentation, and the `meta` suffix has to be explained on every review.

### A2: `credentials` collection, metadata on the item, value at `secret` (CHOSEN)

- Good: metadata surfaces cannot carry a value because the value is not part of that resource; the invariant is structural.
- Good: one schema for the list item and the point record (D8); `ETag` naturally available to metadata readers (D4).
- Good: every disclosing route is its own path, so gateway policy, rate limits and audit selectors need no body inspection (D5). Not a single glob, though: `/credentials/*/secret` misses `POST /credentials:read-secrets`, so both addresses must be named — see Consequences.
- Good: vocabulary matches the domain and converges with the existing `credentials-storage` service, which matters if the gear becomes its successor.
- Bad: renames the collection and moves the value read; every HTTP consumer of a value changes.

### A3: `secrets` collection, value at `value`

- Good: the same structural benefit as A2 with a smaller rename — the collection name survives.
- Bad: muddled vocabulary, in which "secret" names the record and "value" names the secret; every reader has to hold that inversion in mind.
- Bad: keeps a name that will read oddly next to `credentials-storage` if the two converge.

### B1: value only at its own address (CHOSEN)

- Good: privileges, audit, rate limits and cache policy align with paths; fixed schema per address.
- Bad: more endpoints; a `POST` for a read; bulk clients handle partial results.

### B2: one address, value opted in by query parameter

Evaluated in its strongest form: the item returns metadata by default and the value only for `?show-secret=true`.

- Good: safe by default, and fewer endpoints; the metadata default carries the `ETag`.
- Good: intent is explicit and lands in access logs, since query strings are logged.
- Bad: one address, two response schemas; `value` becomes optional in the generated spec and typed SDKs lose the guarantee (D8).
- Bad: gateway policies and rate limits match paths, so value reads cannot be scoped or throttled separately without query-aware rules the gateway does not have (D5).
- Bad: the action to evaluate is selected from a client-supplied flag rather than from the operation.
- Compatibility: breaks every current value consumer until each adds the flag — which under D1 is not itself disqualifying; D8 and D5 are.

### B3: one address, value opted in by request header

- Good: URLs stay clean.
- Bad: invisible to path-based policy and easy to miss in access logs, which is exactly the signal audit needs (D5).
- Bad: an SDK or proxy default could set it inadvertently, turning a metadata read into a disclosure.
- Bad: correct caching would require `Vary` on a custom header; in practice everything becomes uncacheable.

### B4: response shape decided purely by permissions

- Good: no flags at all, and the `ETag` reaches metadata-only roles.
- Bad: silent degradation. An application whose grant was revoked receives `200` without the field and fails later, elsewhere, or sends mail with an empty password. An explicit refusal fails loudly at the boundary.
- Bad: three response states instead of two, and existence becomes observable to every metadata holder, so the anti-enumeration rule stops being one sentence (D3).
- Bad: two PDP evaluations on the hottest path (D7), and audit becomes result-dependent rather than operation-dependent (D5).

### B5: values on the paginated metadata collection

- Good: one round-trip for "everything I may read", with the filtering the collection already has.
- Bad: paginated, filterable and value-bearing at once is a walkable catalogue dump — the one shape D6 exists to forbid.
- Bad: collapses `list_meta` and `read_value` into a single effective privilege (D2).
- Bad: pagination interacts badly with per-item authorization and with the fence; a page would mix served values, fence-failed items and authorization holes, and a cursor would let a client resume the walk.
- Rejected. The legitimate need behind it — "all my SMTP secrets" — is served by the bulk address, which keeps the scoped selector but refuses pagination and caps the result.

## Traceability

- Requirements (shipped): `cpt-cf-credstore-fr-get-secret`, `cpt-cf-credstore-fr-put-secret`, `cpt-cf-credstore-fr-delete-secret`, `cpt-cf-credstore-fr-optimistic-concurrency`, `cpt-cf-credstore-fr-service-retrieve`, `cpt-cf-credstore-nfr-confidentiality`, `cpt-cf-credstore-nfr-tenant-isolation`.
- Requirements (proposed by the same project, added to the PRD in §5.8): `-fr-credential-record`, `-fr-list-credentials`, `-fr-get-credential`, `-fr-write-credential-record`, `-fr-read-secret`, `-fr-write-secret`, `-fr-bulk-read-secrets`, `-fr-authz-action-split`, `-fr-secret-category`, `-fr-inheritance-status`, `-fr-override-type-consistency`; `-fr-suppression` (p2).
- Builds on [ADR-0003](0003-cpt-cf-credstore-adr-value-fingerprint-fence.md): the fence runs per returned value, including per item in a bulk read.
- Depends on the companion ADR for the collection read under the no-projection PEP contract ([ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md)). The PRD non-goal it needed lifted is already narrowed in this change: listing credential records is in scope, and only full-text search over names or values remains a non-goal.
- Pagination and filtering of the credential collection follow `guidelines/DNA/REST/PAGINATION.md`; the bulk secret read deliberately opts out of the cursor machinery while keeping the platform `$filter`, following `POST /usage-collector/v1/records/aggregate` (`gears/system/usage-collector/usage-collector/src/api/rest/routes/usage_records.rs:121-147`) as the precedent for "OData filter, no pagination, non-`Page` response".
- Answers the open questions recorded in `PRD.md:717` / `DESIGN.md:790` (batch retrieval) and the value-exposure half of `PRD.md:720` / `DESIGN.md:793` (metadata list vs anti-enumeration).
- `$`-prefixed parameters must be declared through `OperationBuilderODataExt` (`libs/toolkit/src/api/operation_builder.rs:447`) to satisfy the external `cargo-gears` lint `de0802_use_odata_ext`; `limit` and `cursor` have no helper and are declared manually where they apply.
