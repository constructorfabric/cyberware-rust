---
status: proposed
date: 2026-09-08
---

Created:  2026-09-08 by Constructor Tech
Updated:  2026-09-10 by Constructor Tech

# ADR-0004: Credential Record and Secret Value as Separate Resources

<!-- toc -->

- [Context and Problem Statement](#context-and-problem-statement)
- [Decision Drivers](#decision-drivers)
- [Considered Options](#considered-options)
  - [Axis A — resource modelling](#axis-a--resource-modelling)
  - [Axis B — how a value is asked for](#axis-b--how-a-value-is-asked-for)
  - [Axis C — how a value is written](#axis-c--how-a-value-is-written)
- [Decision Outcome](#decision-outcome)
  - [Resulting surface](#resulting-surface)
  - [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not)
  - [Why `credentials` and not `secrets`](#why-credentials-and-not-secrets)
  - [Why the path key stays a reference, not a UUID](#why-the-path-key-stays-a-reference-not-a-uuid)
  - [Two write verbs on one resource: PUT and PATCH, no POST](#two-write-verbs-on-one-resource-put-and-patch-no-post)
  - [The value-less record: reached by PATCH, never by create](#the-value-less-record-reached-by-patch-never-by-create)
  - [Suppression: `fallback`, the record's policy for having no value](#suppression-fallback-the-records-policy-for-having-no-value)
  - [What a response says about tenants above: `owner_tenant_id` is dropped](#what-a-response-says-about-tenants-above-owner_tenant_id-is-dropped)
  - [Bulk secret read: the collection in value mode](#bulk-secret-read-the-collection-in-value-mode)
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
  - [B6: values on the collection under `$select`, non-paginated and capped (CHOSEN for bulk)](#b6-values-on-the-collection-under-select-non-paginated-and-capped-chosen-for-bulk)
  - [C1: value sub-resource with its own `PUT`/`DELETE`](#c1-value-sub-resource-with-its-own-putdelete)
  - [C2: `PUT` (full replace) and `PATCH` (merge) on the record (CHOSEN)](#c2-put-full-replace-and-patch-merge-on-the-record-chosen)
- [Traceability](#traceability)

<!-- /toc -->

**ID**: `cpt-cf-credstore-adr-secret-value-exposure`

## Context and Problem Statement

**What ships today.** Four addresses, one collection, and the value and its metadata inseparable:

| Method and path | `operation_id` |
|---|---|
| `POST /credstore/v1/secrets` | `credstore.create_secret` |
| `PUT /credstore/v1/secrets/{ref}` | `credstore.put_secret` |
| `GET /credstore/v1/secrets/{ref}` | `credstore.get_secret` |
| `DELETE /credstore/v1/secrets/{ref}` | `credstore.delete_secret` |

Three PDP actions cover all four — `read`, `write`, `delete` — evaluated against the resource type `gts.cf.core.credstore.secret.v1~` and its derived types. `{ref}` is the caller-chosen name, never the row id. There is no collection read at all — the PRD lists listing as a non-goal and DESIGN §4.4 states flatly that "there is no LIST".

Product requirements now introduce readers who must **not** see values, and readers who need several values at once:

- **Integration administrator.** Configures SMTP, provider keys and webhooks for a tenant. Needs the catalogue, the inheritance status of every entry, and the ability to rotate, retarget and reset values. Does not need — and should not have — the plaintext of the credential being rotated.
- **Catalogue and audit view.** "What is configured here, since when, does it expire, is it ours or inherited", with no value anywhere in the response.
- **Runtime application, one credential.** Reads the value of a known name in the tenant it acts for, restricted to the credentials that belong to it.
- **Runtime application, its whole credential set.** A common pattern: a mail service wants *all* credentials that belong to it — "all SMTP secrets", expressed as a selector over type — in one round-trip, at startup or per request, rather than N sequential reads.

Both needs are already recorded as open questions in this gear's own documents rather than invented here: `PRD.md:717` and `DESIGN.md:790` note a future `POST /secrets/batch` for multi-credential retrieval, and `PRD.md:720` / `DESIGN.md:793` note a P2 metadata list endpoint that "must be reconciled with anti-enumeration" and needs a dedicated design pass. This ADR answers the value-exposure half of both.

Two questions must be answered together, because the answer to one constrains the other:

1. **Resource modelling.** Today the entity URL returns the sensitive payload, and the safe view would have to be a suffix on it. Is the addressable thing the *credential record* (with the secret as a sub-resource), or the *secret* (with metadata as a suffix)?
2. **Value exposure.** By which mechanism does a caller ask for a value — a distinct address, a query flag, a header, or implicitly through its permissions — and how is the multi-credential case served without turning any endpoint into a bulk-disclosure primitive?

## Decision Drivers

- **D1 — correctness over continuity.** The target shape is chosen on its merits. Where it breaks something that ships today, the break is stated per surface in [Backward Compatibility by Mode](#backward-compatibility-by-mode) and paid for during rollout; it is not designed around. Continuity is an output of this ADR, never an input.
- **D2 — three distinct privileges.** Enumerating entries, reading one record's metadata, and reading a secret value have different blast radius and must be separately grantable.
- **D3 — a refusal is indistinguishable from absence.** Reads outside the caller's grants surface as the canonical 404, including per item inside a bulk response (`nfr-tenant-isolation`).
- **D4 — a value-blind writer must still get a CAS validator.** Writes require `If-Match` (`fr-optimistic-concurrency`) and the strong validator is an HTTP header, so some response a metadata-only role may call has to carry the `ETag`. This is a guarantee for a writer that also holds `read`. A writer holding no read action at all obtains no validator on its own and writes last-writer-wins, or guarded against a validator it was handed; that is the provisioning injector's contract (see the roles table), not a gap.
- **D5 — the point value read and every write keep their own semantics; on the collection, value disclosure is switched on by `$select` and is bounded by cap, per-item authorization and audit rather than by path.** The gateway matches route policies on **paths**, and for the point read and for writes audit records "this subject read/wrote this value" from the operation rather than by inspecting bodies wholesale. (Reads only: writes are authorized and audited by inspecting the body — see [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not).) The collection cannot make the same promise: a value read and a catalogue read share one path there, so the bound has to be the cap, per-item authorization and an audit selector on `$select` instead of on the route.
- **D6 — bulk reads may never exceed the caller's own read scope, and may never be walked.** A bulk value read is acceptable when the selector can only ever match what the caller is already entitled to read; it is unacceptable when it can be paginated through the catalogue, or when it silently truncates.
- **D7 — bounded authorization cost on the hot path.** Applications read values continuously; the decision must not add PDP evaluations per request just to decide what to include.
- **D8 — one shape per address.** A response schema must not depend on who asked, so that generated specs and typed SDKs carry a real guarantee. `$select` is sparse projection of one schema, not a second schema: the fields returned narrow, the type does not.

## Considered Options

### Axis A — resource modelling

- **A1** — `secrets` collection; the value lives on the item (`/secrets/{ref}`), metadata is a suffix (`/secrets/{ref}/meta`).
- **A2** — `credentials` collection; metadata lives on the item (`/credentials/{ref}`), the value is a sub-resource (`/credentials/{ref}/secret`).
- **A3** — `secrets` collection; metadata on the item, value at `/secrets/{ref}/value`.

### Axis B — how a value is asked for

- **B1** — only at its own address (the sub-resource).
- **B2** — one address, value opted in by query parameter (`?show-secret=true`).
- **B3** — one address, value opted in by request header.
- **B4** — one address, the `value` field present if and only if permissions allow.
- **B5** — values carried by the paginated metadata collection (`?show-secrets=true`).
- **B6** — values on the collection only under `$select=secret`, non-paginated and capped.

### Axis C — how a value is written

- **C1** — the value sub-resource has its own write verbs: `PUT /credentials/{ref}/secret` (set-once via `If-None-Match: *`, rotate via `If-Match`) and `DELETE /credentials/{ref}/secret`.
- **C2** — no value write verbs; a value is written only as part of a write to the record: `PUT /credentials/{ref}` (full replace, `value` required) and `PATCH /credentials/{ref}` (RFC 7396 merge, `value` optional, `value: null` removes it).

## Decision Outcome

**Chosen: A2 + B1 for the point read, B6 for the bulk read, C2 for writes.** The addressable entity is the **credential record**, which never carries the secret; the **secret value is read at its own sub-resource for a single reference, and on the collection under `$select=secret` for several at once**; and a value is **written** only through the record's two write verbs, `PUT` (full replace) and `PATCH` (merge) — there is no value write address.

The deciding arguments for A2 + B1 + B6 are D8 (metadata surfaces cannot carry a value because the value is not part of that resource — a URL fact rather than a test invariant, and `$select` narrows the fields a response carries without inventing a second shape), D5 (the point read and every write stay path-based; the collection's value mode trades that for a cap, per-item authorization and an audit selector on `$select`), D2 and D6 — all of them about *reads*. C2 answers a different question, how a value is *written*, and is decided in [Two write verbs on one resource: PUT and PATCH, no POST](#two-write-verbs-on-one-resource-put-and-patch-no-post): atomic create and atomic suppression outweigh the cost of deriving a write's action from its body.

### Resulting surface

At a glance, six addresses replacing the four above:

| Method and path | Returns | PDP action |
|---|---|---|
| `GET /credstore/v1/credentials` | records (metadata), paginated; with `$select` containing `secret`, the selected items' values instead — capped, not paginated | `list`, or `read_secret` per distinct type in value mode |
| `GET /credstore/v1/credentials/{ref}` | one record (metadata), carries the `ETag` | `read` |
| `PUT /credstore/v1/credentials/{ref}` | create or replace the whole credential — record and value together | `write` and `write_secret` |
| `PATCH /credstore/v1/credentials/{ref}` | apply a partial change to the record, the value, or both | `write` and/or `write_secret`, by body |
| `DELETE /credstore/v1/credentials/{ref}` | delete the record | `delete` |
| `GET /credstore/v1/credentials/{ref}/secret` | the value | `read_secret` |

Read against the four shipped addresses in the Context above, three things changed: the collection is named for the record rather than the payload, the value's *bulk read* moved onto the same collection under `$select=secret` while a single value still has its own address, and the three actions on the shipped `secret.v1~` type become six on the renamed `credential.v1~` type — plain verbs for the record, `_secret`-suffixed verbs for the value — with every read address answering to exactly one action outside value mode, and every write address deriving the action(s) it requires from the body. The same table with the headers, preconditions and cache rules each address carries:

| Address | Returns | PDP action | Notes |
|---|---|---|---|
| `GET /credstore/v1/credentials` | credential records (metadata), paginated; in **value mode** (`$select` containing `secret`) the selected `Credential` fields plus `secret` for the matched items, capped and unpaginated | `list`; `read_secret` per distinct type in value mode | `Cache-Control: no-store`, because the body varies by tenant and subject; OData filter/order per `guidelines/DNA/REST/PAGINATION.md`. `$select` allowlist: the `Credential` fields (`reference`, `type`, `sharing`, `status`, `fallback`, `expires_at`, `inheritance`, `version`, `updated_at`) plus `secret`; any other name → 400; without `$select` the item is the full `Credential`. Selecting `secret` switches the request to **value mode**: `read_secret` replaces `list` per distinct type, `limit` and `cursor` are rejected (400) rather than silently accepted, `$orderby` is rejected, the result is capped (fetched as cap + 1, never counted) and fails closed with `400 TOO_MANY_MATCHES` above it, an item the caller may not read is omitted rather than reported, and one audit record is written per value returned — see [Bulk secret read: the collection in value mode](#bulk-secret-read-the-collection-in-value-mode) |
| `GET /credstore/v1/credentials/{ref}` | one `Credential` (metadata) | `read` | carries the `ETag` — the CAS validator source (D4): a strong `"<id>.<version>"` whenever the caller's tenant holds a row under the reference — `declared` or `active`, even while the effective value is inherited — and a weak opaque `W/"…"` only when it holds none (see "What a response says about tenants above"); `Cache-Control: no-store` |
| `PUT /credstore/v1/credentials/{ref}` | — (201 on create with `ETag`, 204 on replace) | `write` and `write_secret` | full replace of the **whole credential — record and value together**: `type` (required on create, immutable after), `sharing` (required), `expires_at`, `fallback` (default `inherit`), and `value` (**required** — a `PUT` always carries a value). Preconditions carry the intent: `If-None-Match: *` create → 201 with `Location` and `ETag` (record and value written atomically: value under a fresh `value_id`, then the row created in one transaction — [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)), `If-Match: "<id>.<version>"` guarded replace → 204, `If-Match: *` last-writer-wins → 204, no precondition → 400, failed precondition → 409. A `PUT` always writes the value and always bumps `version`, even for identical bytes — it never produces a `declared` record and it never takes the no-op shortcut described below |
| `PATCH /credstore/v1/credentials/{ref}` | — (204, `ETag`) | `write` and/or `write_secret`, by body | RFC 7396 JSON Merge Patch (`Content-Type: application/merge-patch+json`): a field present is applied exactly as `PUT` would apply it, a field absent is left untouched. `value: null` **removes the value** — the record becomes `declared` (status 4), the backend entry is deleted. `If-Match` required (`"<id>.<version>"` or `*`); `If-None-Match: *` is not meaningful on `PATCH` → 400; never creates — no own record → 404. No-op rule: a `PATCH` that carries no `value` key and whose metadata equals the current record is a no-op (204, same `ETag`, no bump); a `PATCH` that carries the `value` key, string or `null`, always writes and always bumps |
| `DELETE /credstore/v1/credentials/{ref}` | — (204) | `delete` | `If-Match` required; releases the reference |
| `GET /credstore/v1/credentials/{ref}/secret` | one `Secret`: the value plus `reference`, `type`, `expires_at` | `read_secret` | `Cache-Control: no-store`; audited; carries the record's `ETag` so a self-rotating caller needs no second read |

### Why the value has its own read address, and why writes do not

The table above spends six addresses where four would do. That is the cost, and each of the four reasons below is on its own sufficient to pay it.

**One address, one schema.** A response body is fixed by the URL, not by who asked or by what was sent. A generated client gets one type per address; an OpenAPI reader sees one shape; a reviewer checking "can this path emit a secret" reads the schema rather than the handler. The alternative — one address whose body sometimes carries a value — makes the schema a function of the caller's grants, which is untypeable, and turns "does this leak" into a question about runtime state instead of about a document.

**One address, one PDP action — for reads.** Route policy at the gateway, rate limits and audit selectors all match on paths, and the audit record has to say "this subject read this value" as a property of the operation rather than of the payload. Every read address above — `list`, `read`, `read_secret`, the bulk `read_secret` — answers to exactly one action, and D5 rests on it. Writes deliberately break the rule: authorization of a write is decided by the body — metadata keys present require `write`, the `value` key present (string or `null`) requires `write_secret`, both present require both, evaluated before any side effect — because writes are not the hot path D5 protects, no write address ever discloses a value by being reachable, and audit reads the same body the authorizer read, so "record written" / "value written" / "value removed" is exactly as attributable as a path-keyed record would be. Deriving a privilege from a payload is the shape of an authorization bug on a *read* address; on a write address it is what lets one request create or edit a record and its value atomically — see [Two write verbs on one resource: PUT and PATCH, no POST](#two-write-verbs-on-one-resource-put-and-patch-no-post).

**One address, one intent.** `PUT` is a whole-value replace: fields absent from the body reset to their defaults. A merged `PUT` — record and value in the same whole-replace body — really does face the question this argument raises: change `sharing` and omit the value, and does the value get cleared or preserved? Clear it and an innocent metadata edit destroys a credential; preserve it and `PUT` is no longer a whole replace, so the same body means "reset" for one field and "leave alone" for another. That problem is real, and it is `PUT`'s alone — a whole-replace verb cannot express "touch this field, leave that one", by definition. It is not, however, an argument for a second *address*: it is an argument for a second *verb*. `PATCH` (RFC 7396 JSON Merge Patch) answers it directly — present means replace, absent means untouched, `null` means remove — without inventing bespoke semantics: a field the value-blind integration administrator never had to resend stays exactly as it was, because it was simply absent from the merge body. The value-blind metadata edit — the whole point of the split — is expressible with `PATCH {"sharing": …}` under `write` alone; the administrator never constructs a body that touches the value because `PATCH` never asks it to.

Rotation is why a full-replace hazard would have mattered even to a caller that *can* see the value. A rotator holding `read` and rotating with a whole-body `PUT` sends the metadata back verbatim to change only the value, and under the `If-Match: *` this ADR keeps for provisioning flows that hold no validator, a routine rotation can then overwrite a concurrent `sharing` or expiry change with the stale copy it read a moment earlier. `PATCH {"value": …}` avoids the read-modify-write entirely: the body touches the value and nothing else, so no metadata field is ever put at risk by a rotation that never meant to touch it.

**One address, one store — for reads; a write may span both, by design.** The two resources are not a modelling preference layered over one row; they are already two stores. ADR-0001 put the metadata in the gear's own `credstore_secrets` table and the value in a backend plugin behind `CredStorePluginClientV1`, no transaction spans the two directly — a value write follows the [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md) write protocol, with a gc table and a periodic maintenance job rather than a saga — and the fence of ADR-0003 remains as an integrity check. Every *read* address above maps onto exactly one store: a record read is one indexed query and never touches the backend, so the catalogue stays up when the vault is down; a value read is a backend round-trip plus a fence check. A *write*, though, may legitimately touch both: `PUT` always does — the value is written under a fresh `value_id` first, then the row is created or switched in one transaction ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)) — and a `PATCH` touches whichever store its body names, one or both. That is not the boundary dissolving: the write protocol is still the only thing that ever writes the backend, and `declared` is still the honest name for a metadata row with no backend entry — `value_id IS NULL`, and under ADR-0006 that is now the only value-less state a row can be in, since out-of-band seeding is withdrawn. The fence remains as an integrity check against out-of-band corruption — not because the two stores can still disagree about which bytes a row means; under [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md) they cannot. This argument still settles Axis A — that the record and the value are two resources with two representations — but not Axis B: a flag on a single read address could still route to the backend on demand, and it is the three read arguments above that rule that out. It settles nothing about Axis C; that is decided in [Two write verbs on one resource: PUT and PATCH, no POST](#two-write-verbs-on-one-resource-put-and-patch-no-post).

Suppression is a field of the record, `fallback`, written with the record itself; it has no address of its own (see Suppression below).

**Two representations, and what each carries.** `Credential` is the record: `reference`, `type`, `sharing`, `expires_at`, and two fields that answer two different questions. `status` describes the **caller's own row**: `none` when the caller's tenant holds no row under the reference, `declared` or `active` otherwise — never a saga state, never `provisioning` or `deprovisioning`, which are invisible to every read. `inheritance` (`own` / `inherited` / `overridden` / `suppressed`) describes the **effective row** the reference resolves to, which need not be the caller's own. The two can disagree, and when they do the combination is the point: `inheritance: inherited` next to `status: declared` reads as "you removed your own value here, and the value you get today is an ancestor's". `fallback` (`inherit` / `none`) is shown for the caller's own record only, and only it — an ancestor's policy is not the caller's to see, any more than its `version` is. `version` and `updated_at` likewise describe the caller's own row and are present whenever `status` is not `none`, including when `inheritance` says the effective value comes from elsewhere. `Secret` is the value with exactly what is needed to use it: `reference`, `type` (a consumer must know whether it is parsing a `basic_auth` object or an `api_key` string), `expires_at` (when to come back), and the value itself; the record's `ETag` travels in the header so that a caller which reads and rotates its own credential never needs the record address. Nothing administrative rides along with a value: `sharing`, `inheritance`, `status` stay behind `read`. That is what keeps `read_secret` and `read` two different privileges instead of one nested inside the other, and it is why the question "should the value address require both actions" never arises — each address discloses one representation, and one action covers it. The `PUT`/`PATCH` **body** is a different thing again: the writable subset of `Credential` plus `value`. A body is never a representation the server returns, so D8 — one shape per address — is a statement about responses, and a request body answering to two actions at once does not weaken it.

### Why `credentials` and not `secrets`

Under A2 the collection name has to describe the record, not the payload. "Credential" is the record — a named, typed, tenant-scoped entry with sharing, version and expiry; "secret" is the sensitive value it holds. That vocabulary is the one this project already uses when talking about the two read classes, and it removes the sentence A1 forces you to write: *"`GET /secrets` returns no secrets."*

It also converges with the platform: the Constructor service that already implements a version of this domain is `credentials-storage`, whose collections are `credential-definitions` and `credentials`. If the gear becomes its successor, a shared noun makes the migration and any compatibility facade legible instead of a translation exercise.

**The GTS type follows the noun.** A permission is a pair of resource type and action, and the resource type is the GTS type of the entity. Leaving it at `gts.cf.core.credstore.secret.v1~` while the collection says `credentials` would put two nouns for one thing into every policy — "on resource *secret*, action *read the credential*" — and would leave the type naming the payload rather than the entity it types: a type's traits (`allow_sharing`, `expirable`, `value_schema`) describe the credential as a whole, not its bytes. So the base type is renamed to `gts.cf.core.credstore.credential.v1~`, and every derived type follows (`…credential.v1~cf.core.credstore.basic_auth.v1~`). `SECRET_RESOURCE_TYPE`, the seeded catalog and `GENERIC_TYPE_UUID_STR` change with it; the stored `secret_type_uuid` is the v5 UUID of the type id, so every stored value changes too. Nothing outside the credstore crates references the old id. While the gear has no production rows this is a constant change; afterwards it is a data migration and a re-registration of every custom type, which is why the rename is decided here and not later.

One consequence is worth stating because it removes a rule this ADR would otherwise need. Shipped permissions target `secret.v1~`; the new surface authorizes against `credential.v1~`. No shipped grant can match any new operation, so the authorization break below is structural rather than a convention ("the old `read` is not a synonym") that reviewers would have to police.

**"Credential" is used in its broad sense**: evidence of identity or authority — passwords, API keys, tokens, OAuth clients, certificates, webhook signing secrets. Nearly every type in the catalog is one. `generic` is the documented exception for opaque material, kept because a store that types its entries still needs a type for the untyped, not because the noun fits it.

With the type renamed, every layer says the same two words: the type, the `credentials` collection, the `Credential` and `Secret` schemas, the `read`/`read_secret` actions, and the SDK's `get`/`get_secret`.

### Why the path key stays a reference, not a UUID

`{ref}` remains the caller-chosen `SecretRef` (`[A-Za-z0-9_-]+`), not the row UUID. The reference is the stable name a consumer hard-codes; the row id is a *generation* id, minted fresh whenever a credential is deleted and recreated, and it is deliberately kept out of response bodies so that CAS validators have exactly one source (the `ETag`). A UUID in the path would force consumers to resolve name → id on every call and would make a recreated credential a different URL, breaking the one property applications rely on.

### Two write verbs on one resource: PUT and PATCH, no POST

The record has two write verbs, `PUT` and `PATCH`; the value has none of its own — see [C2](#c2-put-full-replace-and-patch-merge-on-the-record-chosen). Intent inside each verb is still carried by RFC 7232 preconditions:

| Intent | Request |
|---|---|
| create the record and its value, fail if the record exists | `PUT /credentials/{ref}` (`value` required) + `If-None-Match: *` → 201, or 409 if the caller's tenant already holds a record under that reference |
| replace the whole credential, only if unchanged since I read it | `PUT /credentials/{ref}` (`value` required) + `If-Match: "<id>.<version>"` → 204, or 409 |
| replace the whole credential, last writer wins | `PUT /credentials/{ref}` (`value` required) + `If-Match: *` → 204 |
| edit some metadata, leave the value alone | `PATCH /credentials/{ref}` with any of `sharing`, `expires_at`, `fallback` + `If-Match` → 204 |
| rotate the value, leave the metadata alone | `PATCH /credentials/{ref}` `{"value": "…"}` + `If-Match` → 204 |
| remove the value, keep the record | `PATCH /credentials/{ref}` `{"value": null}` + `If-Match` → 204; the record is `declared` again and its `fallback` decides what resolves |
| suppress an active own credential, atomically | `PATCH /credentials/{ref}` `{"fallback": "none", "value": null}` + `If-Match` → 204; one row update, no window (see Suppression) |
| suppress an inherited credential here, with no value of its own to publish | not a single request: `PUT` always requires a `value`, so a tenant with nothing of its own has no one-request path; it `PUT`s a record with some value, then `PATCH`es `{"fallback": "none", "value": null}` — an accepted two-request cost, see [Revisit Triggers](#revisit-triggers) |

**"If it exists" is judged against the caller's own tenant.** `GET /credentials/{ref}` may well return 200 for a reference the caller has never declared — an ancestor's `shared` credential is a current representation of that URL — and RFC 9110 read literally would then fail `If-None-Match: *`. That reading would make the override flow unexpressible: the case in which a tenant declares its own record is precisely the case in which the reference already resolves to an ancestor's. So the create-only precondition asks "does *my* tenant hold a row under this reference", inherited representations do not count, and the deviation is stated here rather than discovered in a test.

**One status code for every failed precondition: 409.** RFC 9110 offers 412 for a failed `If-None-Match` and the platform maps a version conflict (`OPTIMISTIC_LOCK_FAILURE`) to 409; using both would make "your precondition did not hold" two different codes depending on which precondition it was. The platform mapping wins, and the reason code in the body says which precondition failed.

`PATCH` was rejected in an earlier draft of this ADR: "a partial-update verb needs its own merge semantics and its own conflict rules on top of the ones `PUT` already has, and it invites the pattern where a client mutates one field without ever holding the whole record — which is precisely how a `sharing` change gets made without noticing the expiry it left behind." That objection does not survive contact with RFC 7396. The merge semantics are not bespoke: present means replace, `null` means remove, absent means untouched — a standard this ADR adopts rather than invents, which answers the first half of the old objection outright. The second half — a client can mutate one field without holding the whole record — is real, and this ADR now accepts it as the price of letting a value-blind administrator change `sharing` (or any other metadata field) without ever supplying a value, which is exactly the case ["One address, one intent"](#why-the-value-has-its-own-read-address-and-why-writes-do-not) said a merged `PUT` could not serve. `PATCH` is what makes a merged write address work at all: it is the reason `write` and `write_secret` can share one resource without either being able to destroy or force-resend what the other manages. Without `PATCH`, the two-address design (value at its own sub-resource, [C1](#c1-value-sub-resource-with-its-own-putdelete)) was necessary — not merely convenient — because `PUT` alone could never let a value-blind edit and a value write coexist safely on one resource.

`PUT` on the record remains a whole-*credential* replace: fields absent from the body are reset to their defaults, `type` and `sharing` and `value` included — exactly as today's shipped value `PUT` already clears an omitted expiry, now extended to the whole entity.

`POST` on the collection is absent too, because `PUT` with `If-None-Match: *` already expresses create-only, keeps the write idempotent, and puts the name in the URL where it belongs rather than in the body. What `POST` would have bought over `PUT` — a single atomic request that creates the record and its value together — `PUT` now provides directly, since a `PUT` always carries a `value`; there is nothing left for a separate creation verb to do.

The record's `type` remains immutable: a `PUT` whose body names a different type is rejected rather than applied, and a `PATCH` naming a different `type` is refused the same way — `type: null` on `PATCH` is a 400 too, since it is a NOT NULL column and immutable besides. "Whole-credential replace" on `PUT` therefore covers the mutable fields only: `sharing`, `expires_at`, `fallback`, and `value`.

**Versioning.** There is one resource, `Credential`, one monotonic `version`, and one `ETag` — not two resources sharing a validator, because there is no longer a second resource. Every write reads that same validator and every successful write bumps it: `PUT` unconditionally, `PATCH` whenever it changes anything. A guarded `PATCH {"value": …}` and a guarded `PATCH {"sharing": …}` contend for the same `If-Match`, so a concurrent rotation can invalidate a pending metadata edit and vice versa — conservative, since either write can be rejected because the other landed first, but it keeps one validator and one counter in the system rather than two that can drift. Independent per-field `ETag`s would be more precise and are the obvious extension if the false conflicts ever hurt.

**A metadata-only `PATCH` that changes nothing bumps nothing.** `PATCH /credentials/{ref}` carrying no `value` key, whose metadata fields — after normalization, expiry included — equal the current record, returns 204 with the unchanged `ETag`, leaves `version` and `updated_at` where they were, and writes no row. A retried or idempotently re-sent metadata edit therefore cannot invalidate a concurrent writer's validator or fake a change in the catalogue. Validation still runs first: a re-sent record whose metadata now fails validation is refused, not waved through as a no-op. `PUT` never takes this shortcut: a `PUT` body always carries a `value`, so "a `PUT` that changes nothing" would have to re-raise the equality-oracle problem the next paragraph rules out, and this ADR does not special-case the metadata half of a `PUT` to no-op while its value half writes unconditionally — one request, one outcome.

**A body carrying the `value` key never takes that shortcut.** `PUT` (always) and `PATCH {"value": …}` (whenever the key is present, string or `null`) write the backend and bump the version every time, identical bytes included. Deciding "unchanged" would mean comparing the submitted value against the stored fingerprint, and the outcome would be observable — through the version, the `ETag`, or `updated_at`, all readable under `read` — by a caller holding `write_secret` and not `read_secret`. That caller could then submit guesses and watch whether the version moved: an equality oracle on a value it is not allowed to read, which is exactly the disclosure class this ADR exists to close. Skipping the backend write would add a timing channel on top — and under immutable versions a re-write of identical bytes is simply a new version, which is what a client that suspects a corrupted entry does deliberately ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)). The value path stays oblivious to content equality, on `PUT` and on `PATCH` alike.

### The value-less record: reached by PATCH, never by create

Atomic create is back. `PUT /credentials/{ref}` with `If-None-Match: *` creates the record and writes its value in one request: the value is written under a fresh `value_id` first, then the row is created in one transaction ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)) — there is no two-step create, no lost second request, no window between "the record exists" and "the record has a value". A `PUT` that lacks a `value` is rejected with 400: the field is required, not optional-then-completed-later.

`declared` — a record with no value — still exists, but it is reached exactly one way: `PATCH /credentials/{ref} {"value": null}` on an existing record. It is a deliberate state a caller chooses, not a half-finished create: "value removed, record kept, `fallback` says what resolves" (see [Suppression](#suppression-fallback-the-records-policy-for-having-no-value)). The properties that make it safe to have around still hold, restated against the one way it is now reached:

- **A `declared` record does not resolve and does not shadow — while its `fallback` is `inherit`, the default.** "Does not resolve" means it is not a candidate, not that the read returns 404: the walk continues past it, so a `GET .../secret` returns an ancestor's `shared` value when the chain offers one and the canonical 404 only when nothing in the chain does. A tenant that removes its own value must not silently break inheritance that was resolving before the removal. A record whose `fallback` is `none` still does not resolve *for itself* — it carries no value to serve — but it **does** stop the walk rather than let it continue, which is the suppression case (see Suppression below) and the one place this bullet's rule does not extend past this record. This is the one rule with a claim on a *second* surface: the collection read reduces a reference's rows to the one a value read would resolve, so its reduction has to skip a `declared`/`inherit` local row in favour of a resolvable inherited one, or the listing would report the empty local record while the point read serves the ancestor's value ([ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md) "Reducing a reference to one item").
- **It is a distinct state, not "unfenced", and it is stored.** Today a `NULL` value fingerprint means "seeded out of band, serve on trust". A `declared` record has no value at all, so the two cases must be told apart explicitly — by a lifecycle state on the record, not by the nullability of the fingerprint. Concretely: a fourth `status`, `declared`, widening the `CHECK (status IN (1, 2, 3))` the initial schema carries. Naming the state without giving it a column would leave every predicate that has to distinguish it — resolution, the periodic maintenance job's expiry pass, the collection read — resting on application logic over a value the metadata row does not hold; DESIGN §6.1 sets those predicates out. It is deliberately *not* a new nullable marker: `status` is already the lifecycle column, and the job's expiry pass that must exclude the state needs no change at all, while the resolution predicate gains exactly one disjunct, `status = 4 AND fallback = none`, for suppression (see Suppression below).
- **Neither pass of the periodic maintenance job touches it.** There is no reaper under [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md): with `declared` as its own status this falls out rather than being enforced — the job's expired-row pass selects `active` rows past their `expires_at`, and its gc drain operates on `credstore_value_gc` entries, not on `credstore_secrets` rows — a `declared` record, `value_id IS NULL`, has no `expires_at` match and no gc entry naming it, so it is a candidate for neither: it was never abandoned mid-write, its value was removed on purpose by a `PATCH` that succeeded and committed. A record can sit `declared` indefinitely, waiting for the next value write or none at all, and no job run ever times it out.
- **The state is reachable again, on purpose, and crash-safely.** `PATCH {"value": null}` is one transaction: it sets `value_id = NULL`, `declared`, clears the fingerprint and enqueues the old version for garbage collection; the backend is touched only after the commit, to delete a version nothing points to ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)). The record keeps its metadata, its `sharing`, its `fallback` and its reserved reference; what the reference means next is decided by that `fallback`, not by the `PATCH`: `inherit` lets an ancestor's `shared` value resolve again for this tenant and, per `sharing`, its descendants; `none` blocks it at this row instead.

### Suppression: `fallback`, the record's policy for having no value

Every record carries a **`fallback`** column, its policy for the time it holds no value: `inherit` (the default) or `none`. `inherit` is today's `declared` behaviour restated as a choice rather than the only option — while the record has no value, the reference stays transparent and resolution walks on to the nearest ancestor's `shared` value, exactly as [The value-less record](#the-value-less-record-reached-by-patch-never-by-create) above already establishes. `none` closes that transparency: while the record has no value, the reference resolves to nothing at all — the walk stops at this row instead of continuing past it, and a value read of it, or of any descendant reached through `shared`, gets the canonical 404. Storage is `fallback SMALLINT NOT NULL DEFAULT 1 CHECK (fallback IN (1, 2))`, `1` = `inherit`, `2` = `none` — a SMALLINT code in the database, a string name on the wire, the convention this platform already applies to a two-valued policy column: account-management's `conversion_requests.target_mode` stores exactly such a choice as `SMALLINT CHECK (target_mode IN (0,1))`, and types-registry documents the rule for every gear to follow in the header of `gears/system/types-registry/types-registry/src/infra/storage/entity/enums.rs`.

`fallback` is a field of the record, set through `PUT /credentials/{ref}` or `PATCH /credentials/{ref} {"fallback": …}`, always under the `write` action — the same action that sets `sharing`, and for the same reason. Resolution decides what a reference means from the metadata row, before the backend is ever touched ("one address, one store" above), and `fallback` is exactly that kind of decision — a resolution policy, not a value. It never reads, deletes or replaces a value on its own: setting it on a record that already holds one changes nothing observable unless the same request also touches `value`, and it can only ever be set on the caller's own row, because `write` never reaches an ancestor's record. `write_secret` cannot set or lift it, for the same reason it cannot touch `sharing` — it is the value-only action, and deciding what happens in the *absence* of a value is a metadata question by construction. `fallback: null` on a `PATCH` is refused with 400, like every other NOT NULL metadata field: lifting or arming suppression always names an explicit value, `inherit` or `none`.

What each combination means for resolution, per row:

| Own row | `fallback` | What resolves here and below (per `sharing`) |
|---|---|---|
| `declared` | `inherit` | ancestor's value, if any |
| `declared` | `none` | 404 — the walk stops here |
| `active` | either | own value — `fallback` not consulted |

The resolution predicate becomes `status = 2 OR (status = 4 AND fallback = 2)`: a `declared` row with `fallback = 1` still never resolves and never shadows, unchanged from today; a `declared` row with `fallback = 2` becomes a candidate, and — nearest still wins — a winner with no value yields 404 rather than letting the walk fall through to whatever sits behind it. `inheritance: suppressed` names exactly that outcome: the winning row is a value-less record with `fallback: none`, whether that row is the caller's own or an ancestor's `shared` row reached by a descendant.

Suppression propagates exactly as any other row does, through `sharing`. A `declared`/`none` record with `sharing: shared` blocks resolution for its whole subtree — every descendant's walk stops at it — while `sharing: tenant` blocks only the tenant holding the row. An ancestor that suppresses for its subtree tells a descendant nothing about *why*: a descendant reading `inheritance: suppressed` learns that the nearest resolvable row carries no value and blocks the walk, not which tenant put it there or what, if anything, sits behind it — the same thing a plain 404 already withholds.

Writing a value to a suppressed record is not a conflict. `PATCH {"value": …}` on a `declared`/`none` row makes it `active`, exactly as it would on a `declared`/`inherit` row, and the record now serves its own value and reads `overridden` — `fallback` stays stored but stops being consulted, per the table above. So does a `PUT` that replaces the whole record with a fresh value: it always writes a value, so it always leaves the row `active` regardless of the `fallback` it carries. Lifting suppression without supplying a value is `PATCH /credentials/{ref} {"fallback": "inherit"}` — metadata only, under `write` alone; the record does not need a value for that write to succeed, which is exactly the case a value-blind administrator needs and a merged `PUT` could not have served.

There is no lost-second-request case to describe: creation is atomic, so a record is never observed between "created" and "has a value" — a caller either supplies `fallback` and a value together in one `PUT`, or reaches `declared` later, deliberately, by removing a value it once held.

Suppressing a *currently active* own credential is one atomic request: `PATCH /credentials/{ref} {"fallback": "none", "value": null}` under a single `If-Match`. `fallback` is stored and armed while a value is present but does not participate in resolution then — the own value always wins over its own record's policy — so the merge patch arms `fallback: none` and removes the value in the same row update; the row goes `active` → `declared`/`none` in one transaction that also enqueues the old version for garbage collection, and the backend delete follows after commit exactly as any other value removal does ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md); see [The value-less record](#the-value-less-record-reached-by-patch-never-by-create)). There is no window to close, because there are no longer two requests between which one could open: the metadata change and the value removal are two keys of one merge-patch body, applied together before either takes effect.

Two alternatives were considered and rejected. A fifth `status` value ("suppressed") cannot express the armed state — active, with suppression waiting for the value to be removed — without a permission crossover: either a `write` that discards the current value to flip the status, or a `write_secret` that sets a policy it has no business touching. `fallback` avoids that because it is orthogonal to `status`: it can be true regardless of whether a value exists, and only its *interaction* with `status = 4` matters to resolution. A tombstone at the value's own address — a marker written by whatever call removes a value, that itself decides future resolution — was rejected for a narrower but sufficient reason: the resolver never reads the backend for candidate rows, exactly one SQL query decides the winner and exactly one backend read serves it ("one address, one store" above), so anything the resolution predicate depends on has to live in the metadata row it already queries; a tombstone at the value's address would have to be mirrored into that row anyway, leaving two sources of truth for one decision, and it would hand `write_secret` an administrative power — deciding what a reference means — that ["One address, one PDP action"](#why-the-value-has-its-own-read-address-and-why-writes-do-not) spends its whole argument keeping out of that action.

**Scenario.** T1 is a partner tenant; T2 is its child; T3 is T2's child.

0. **(T1)** publishes `smtp-default` as `shared`, value `V1`. T2 and T3 hold no row and both resolve `V1`, `inheritance: inherited`.
1. **(T2)** `PUT /credentials/smtp-default` `If-None-Match: *` `{type, sharing: shared, fallback: inherit, value: V2}` → 201. Goal: override the inherited default. T2's row is `active`, `inheritance: overridden`; T2 and T3 now resolve `V2`.
2. **(T2)** `PATCH /credentials/smtp-default` `{"value": V3}` `If-Match: "id.1"` → 204. T2 and T3 resolve `V3`.
3. **(T2)** `PATCH /credentials/smtp-default` `{"fallback": "none", "value": null}` `If-Match: "id.2"` → 204. T2's row is `declared`/`none`, `inheritance: suppressed`; `GET …/secret` in T2 and T3 now returns 404, with no intermediate state that ever served `V1`; T1's record and value are untouched, and T1's other descendants keep resolving `V1`.
4. **(T2)** `DELETE /credentials/smtp-default` `If-Match: "id.3"` → 204. T2's row is gone; T2 and T3 resolve `V1` again, `inheritance: inherited`. **Alternative to step 4**: `PATCH /credentials/smtp-default {"fallback": "inherit"}` `If-Match` → 204 instead of deleting — T2 keeps its record (still `declared`, still reserving the reference) and T2/T3 resolve `V1` the same way.

### What a response says about tenants above: `owner_tenant_id` is dropped

Today's metadata carries `owner_tenant_id`, and for an inherited credential that is the identifier of an **ancestor** tenant. Both it and the `is_inherited` boolean beside it are dropped; `inheritance` (`own` / `inherited` / `overridden` / `suppressed`) is the single field that answers the question either of them answered, and it answers it better — `is_inherited` cannot tell "mine, and nothing above" from "mine, and shadowing something above", which is precisely the distinction that decides what happens if the record is deleted.

The tenant identifier is a different matter, and it is dropped for a reason worth writing down rather than for tidiness.

**The caller has no other way to obtain it.** The gear reads the ancestor chain from the Tenant Resolver, in-process and with barriers ignored, which is a privilege it holds as a gear and the caller does not. The Tenant Resolver publishes no HTTP surface at all — no `RestApiCapability`, no routes, no OpenAPI document — so nothing outside the process can ask it for a chain, and it performs no authorization of its own, trusting the calling gear to have decided. Account Management does expose a tenant's `parent_id`, but one record at a time and under its own PDP: a caller learns its immediate parent only if it may read its own tenant record, and reaching a grandparent needs a grant on the parent's record first. So a chain is walkable only by someone already entitled to each step.

Putting the owning tenant in a credential response bypasses that, and it does so for any ancestor at any depth. The identifier is also the by-product of a privilege the caller does not hold: the gear fetches the chain with barriers ignored (`cpt-cf-credstore-adr-upward-collection-read`), and it may do so because a `shared` value is published downward regardless of barriers. A barrier-respecting traversal stops at a `self_managed` tenant, so a tenant at or below a barrier is never shown the tenants above that barrier; the gear sees them only because it looks past the barrier on the caller's behalf. Nothing here changes what a barrier means — it still isolates a customer's management from its parent, and data published as `shared` still flows down through it, exactly as ADR-0005 states — it only says that what the gear learned by looking past the barrier is not the caller's to keep. Handing it out would be the gear using its trusted position to disclose what the position was granted for, which is the confused-deputy shape.

**It can be reconstructed, by whoever is entitled to.** Nothing is permanently lost. A caller that holds the grants can walk Account Management upward, one authorized read per level, and match a reference against each tenant's catalogue to find where it lives. An operator investigating "where did this credential come from" works with operator rights and can do exactly that. What the removal takes away is the shortcut that skipped the authorization at every level; what it costs is several requests instead of one field, paid by the party that has the rights to make them.

**`version` and `updated_at` go the same way when they would describe an ancestor's row.** Both describe that row's write activity — how many times it has rotated or edited the credential, and when it last did — which is operational detail about a tenant the caller cannot otherwise observe, and neither serves the caller: the CAS validator of a row it does not hold is useless to it, since it cannot write that row, and D4's guarantee is about the caller's *own* row. A weaker class of disclosure than the identifier, but the same shape, and dropping one while keeping the other would be inconsistent. So a `Credential` carries the weak `ETag` — and neither field — **only when the caller's tenant holds no row at all under the reference**: instead of the strong `"<id>.<version>"` it carries a **weak, opaque** validator, `W/"<hmac(id.version)>"` under a key kept for this purpose and nothing else, bound to whichever ancestor's row currently wins. Two properties follow by construction. It still changes exactly when that ancestor writes, so a consumer can still ask "has this changed since I last looked" cheaply under `read`. And RFC 9110 requires the strong comparison for `If-Match`, so a weak validator can never be used to write — a client that tries gets the same refusal it would get for writing an ancestor's row anyway. A caller whose own tenant holds a row — `declared` or `active` — keeps the strong validator and both fields regardless of what the reference resolves to: a `declared` row under a reference that currently resolves to an ancestor's value still carries its own `version`, its own `updated_at`, and a strong `ETag`, because that row, not the ancestor's, is the one it may write. The collection read follows: `updated_at` is present on an item whenever the caller holds a row under its reference and absent when it does not, so a page mixes items with and without it — which is why ADR-0005 offers no ordering or filtering on it.

**Open question for the platform, not for this gear.** This reasoning holds only while the ancestor chain stays unpublished. If a Tenant Resolver HTTP API is added later and exposes `get_ancestors` to callers, the chain becomes obtainable directly and the argument above weakens or disappears — at which point re-adding `owner_tenant_id` would cost nothing and would be a convenience worth having. **This needs confirming with the Tenant Resolver's owners before this ADR is accepted:** is the upward chain intended to stay unavailable to a caller with minimal rights once that gear grows an API, or is it planned to be public? The answer changes nothing about the split itself, only about this one field.

### Bulk secret read: the collection in value mode

The bulk read is not a fifth address. `GET /credstore/v1/credentials` already exists for the catalogue; selecting `secret` in its `$select` switches the same request into **value mode**, reusing the platform's own field-projection mechanism — `guidelines/DNA/REST/API.md` recommends `$select` for exactly this — rather than inventing a bespoke bulk grammar. `$select` is declared through `OperationBuilderODataExt::with_odata_select`: the toolkit already carries it, `toolkit-odata` already parses it, and credstore is its first consumer; a hand-rolled `$`-parameter would trip the `de0802_use_odata_ext` lint, exactly as `$filter` already does elsewhere in this ADR.

**The two selectors are `$filter` forms; there is no request body.**

```http
// explicit — the caller knows the names
GET /credstore/v1/credentials?$filter=reference in ('smtp-default','stripe-key','webhook-signing')&$select=reference,type,expires_at,secret

// scoped — "all my SMTP credentials"
GET /credstore/v1/credentials?$filter=type eq 'gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~'&$select=reference,type,expires_at,secret
```

Only `reference` and `type` are filterable in value mode, `eq` / `in` only — the same two fields [ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md)'s allowlist admits as SQL clamps for the metadata collection, because they are the only fields invariant across a reference's chain ("How authorization applies to a collection", "What stays out of the filter"). `type` takes the **full GTS type id**. There is deliberately no `startswith` on `reference` and no other operator on either field — a prefix scan over names is the enumeration primitive this ADR withholds throughout, value mode included.

**`$select` without `secret` is ordinary sparse projection**, unchanged from any other catalogue read: paginated, `list` per distinct type, no cap, no special rules. **`$select` with `secret` is value mode**, and every rule the withdrawn bulk address had still applies, restated against the collection:

- **`read_secret` per item, evaluated once per distinct type**, exactly as `list` already is for a metadata page — the PDP is not asked once per row (D7).
- **`limit` and `cursor` are rejected outright, 400.** A metadata page silently ignores neither; a value-mode request that carries either is malformed, not merely unpaginated: a paginated value read is a catalogue dump in slow motion, and pagination and values may never coexist (D6).
- **`$orderby` is rejected in value mode**, for the same reason: an ordered, walkable value stream is exactly the shape D6 forbids, ordering included.
- **A hard cap, enforced by fetching `cap + 1`, never by counting** (proposed: 25). No `COUNT` query is ever issued: if a `cap + 1`-th row comes back, the request fails closed with `400 TOO_MANY_MATCHES` and the client narrows the selector; otherwise the rows fetched are the answer. Truncation would both hide credentials from a legitimate caller and turn the endpoint into a drip, so the failure is explicit.
- **The selector vocabulary is deliberately small, and both fields it offers are indexed.** `reference` and `type` are the only filterable dimensions in value mode; the migration that adds `(tenant_id, secret_type_uuid)` is what makes the type-scoped selector anything other than a sequential scan (ADR-0005, "What stays out of the filter").
- **An item the caller may not read is omitted, not reported.** This is an ordinary collection response — no `outcome` envelope, no per-item status — so a row the caller may not read is simply absent from `items`, the same way a catalogue page never lists a row outside the caller's grant. `reference in (…)` and `type eq/in …` are refused identically: the filter found the row, not the caller, and echoing a refused reference back would let `reference in (…)` work as an oracle for names the caller does not otherwise have. Item order follows the collection's canonical sort, `reference` ascending, for both selector forms.
- **`Cache-Control: no-store`, one audit record per value returned.** The gateway can no longer tell a value read from a catalogue read by path alone — both are `GET /credentials` — so the audit selector is `$select` containing `secret`, read the same way authorization reads it: from the query, not from the response body.
- **Per-item fence.** Every returned value is still verified against its row fingerprint (ADR-0003); a fence mismatch drops that item without affecting its siblings.

**Filter in SQL first, reduce the hierarchy in memory.** The SQL step selects candidate rows of the caller's tenant and its ancestor chain under the point-read visibility rules, clamped by `reference` and `type` when `$filter` supplies them; the distinct types present are then authorized with the PDP — `list` in metadata mode, `read_secret` per type in value mode — and a denied type's rows are dropped before reduction. The permitted set is exactly a `secret_type_uuid IN (…)` clamp: the uuid predicate the design expects from the authorization side, computed today from the per-type PDP decisions the gear already makes and pluggable later if the PDP ever hands back a type predicate directly instead. Only then, in memory, are the surviving rows of each reference reduced to one winner — nearest resolvable, a `declared`/`inherit` row never competing, a `declared`/`none` row winning and blocking — and the winner is served (value mode) or listed (metadata mode) if it passed authorization, omitted otherwise. `sharing`, `expires_at` and `fallback` are never SQL clamps, in either mode: they are properties of the winning row, filtered only after reduction decides which row that is. The full mechanics, and why `reference`/`type` are the only sound SQL clamps, are ADR-0005's ("How authorization applies to a collection", "Reducing a reference to one item").

```jsonc
GET /credstore/v1/credentials?$filter=type eq 'gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~'&$select=reference,type,expires_at,secret

200 OK
Cache-Control: no-store
{
  "items": [
    {
      "reference": "smtp-default",
      "type": "gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~",
      "expires_at": null,
      "secret": "…"
    }
  ],
  "returned": 1,
  "cap": 25
}
```

`returned` is the length of `items`, not a database count. There is no `next_cursor` — the type is a flat per-reference result list, so nothing in the contract suggests the result can be continued, exactly as the withdrawn bulk endpoint's response already stated it.

**The usage-collector precedent no longer applies; `$select` is the precedent now.** An earlier draft of this ADR modelled the (now-withdrawn) bulk endpoint's `$filter`-only, non-paginated response on `POST /usage-collector/v1/records/aggregate`. That reasoning is retired along with the endpoint it justified: `$select` is the platform's own sparse-projection mechanism, declared through `OperationBuilderODataExt::with_odata_select` and already parsed by `toolkit-odata`; `guidelines/DNA/REST/API.md` recommends it for field projection generally. Credstore is its first consumer for any handler, and value mode is the first time selecting a field changes which PDP action a request needs — that part is new, and this section, not the guideline, is responsible for it.

### Consequences

- Metadata can never carry a value, because the value is not part of the metadata resource. The invariant is structural, so it cannot be reintroduced by an incautious DTO change.
- The point value read keeps its own path, so route policies, rate limits and audit selectors for it are expressible without body inspection (D5). The bulk read does not: **two surfaces disclose a value**, `GET /credentials/{ref}/secret` on its own path and `GET /credentials` in value mode, and the second is not distinguishable from an ordinary catalogue read by path alone — both are the same `GET /credentials`. What bounds it instead is the cap, per-item authorization (`read_secret` per distinct type) and an audit selector keyed on `$select` containing `secret`, not a route.
- **Type is now the only scope axis, and the escalation path via metadata is gone.** With `category` removed, nothing about a record's metadata decides who may read its value: the type is immutable once set (`fr-override-type-consistency`), so there is no write that moves a credential from one reader's grant into another's the way an editable `category` once could.
- A value-blind administrator rotates by reading the record for its `ETag` and sending `PATCH {"value": …}`; it never touches an address that returns a secret (D4).
- The frequent "give me my whole credential set" pattern is served in one call, with disclosure bounded by the caller's own grant, a cap, and the absence of pagination (D6).
- The list item and the point record share one schema, which is what clients expect and what generated SDKs can type.
- The value carries only its usage envelope (`type`, `expires_at`), so `read_secret` discloses a different representation from `read` rather than a superset of it; the two privileges stay distinct in fact, not only in name.
- Cost: this renames the shipped collection and moves the value read to a sub-resource. Every current consumer of the value changes. That is accepted under D1 and recorded below.
- Cost: value mode moves the bulk read from a `POST` (implicitly uncached) to a `GET` sharing the collection's own address, so intermediary caching is no longer avoided by the method alone — `Cache-Control: no-store` on every value-mode response is now load-bearing, not a belt-and-braces header.
- Cost: a value-mode request performs up to the cap backend reads and fence verifications per request, plus one PDP evaluation per distinct type; it needs its own latency budget, separate from an ordinary catalogue page's.
- Cost: bulk clients must treat a short `items` list as partial success — an omitted reference is not distinguishable from one that was never in scope — which is more to reason about than N point reads that each either succeed or 404.
- Cost: writes derive their required action(s) from the body rather than from the address alone — a deliberate, documented exception to "one address, one action" (see [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not)). Both `write` and `write_secret` are evaluated, when both apply, before any side effect takes place, so a caller missing either fails the whole request rather than partially applying it.
- The value-less record (`declared`) is a deliberate state, reached only by `PATCH {"value": null}` on an existing record — never a half-finished create, since creation is atomic. The domain, the periodic maintenance job, and the resolution rules still have to account for it, exactly as they did before, but there is no window during which one exists by accident.
- Benefit of the same trade: metadata can be edited without touching the value at all, which is what makes a value-blind administrator role possible in the first place; with only a value-bearing `PUT` that role could not change a sharing mode without rewriting the secret.

### Confirmation

- Contract tests: the `Credential` schema returned by `/credentials` and `/credentials/{ref}` outside value mode contains no `value`/`secret` property; the value-mode projection under `$select` is a distinct response type carrying exactly the selected `Credential` fields plus `secret`, never present without `secret` named in `$select`; `value_fp` and `fp_key_id` appear in no schema at all.
- E2E: a role holding `read` and `write_secret` (but not `read_secret`) reads the record, obtains the `ETag`, completes a guarded `PATCH {"value": …}`, and still receives 404 on `GET …/secret` — byte-identical to the 404 for a name that does not exist.
- E2E: `GET /credentials?$filter=reference in ('a','b','c')&$select=…,secret` and `GET /credentials?$filter=type eq '<type>'&$select=…,secret` both enter value mode; a selector matching more than the cap fails the whole request with `TOO_MANY_MATCHES`; an item the caller may not read is omitted from `items` rather than reported, under either selector form; a fence-poisoned item is dropped without affecting its siblings.
- E2E: an application granted `read_secret` on one type receives exactly the credentials of that type for a `$filter` scoped to it, and an empty result for a type it is not granted.
- E2E: a value-mode request (`$select` containing `secret`) rejects `limit`, `cursor` and `$orderby` with 400; a `$select` naming a field outside the `Credential`-plus-`secret` allowlist is rejected with 400 in either mode; a `$select` that omits `secret` stays an ordinary paginated catalogue page, `next_cursor` included.
- E2E: `PUT /credentials/{ref}` with `If-None-Match: *` and a `value` returns 201 the first time and 409 the second; a `PUT` naming a different type is rejected; a `PUT` that omits expiry clears it; a `PUT` without a `value` returns 400; a `PUT` from a caller holding only `write` or only `write_secret` (never both) is refused with 403; a `PUT` whose metadata equals the current record still bumps `version` and writes a new `ETag`, because its `value` is always written.
- E2E: `PUT /credentials/{ref}` with `If-None-Match: *` and a `value` returns 201 for a reference that currently resolves to an ancestor's `shared` credential — the override flow — and the record's `inheritance` reads `overridden` immediately, since the value is written in the same request.
- E2E: `PATCH /credentials/{ref} {"value": "…"}` under `If-Match` rotates an `active` record's value and bumps `version` even when the bytes are identical to the current value; `PATCH {"value": null}` under `If-Match` returns the record to `declared`, deletes the backend entry, keeps its metadata, and the reference then resolves per `fallback`: to the ancestor's `shared` value under `inherit`, to nothing under `none`.
- E2E: authorization of a `PATCH` follows its body. A caller holding only `write` sending `{"sharing": …}` succeeds, but the same caller sending `{"value": …}` is refused with 403; a caller holding only `write_secret` sending `{"value": …}` succeeds, but the same caller sending `{"sharing": …}` is refused with 403; a caller sending a body with both a metadata field and `value` needs both actions or the request is refused before either half is applied.
- E2E: `PATCH /credentials/{ref} {}` is refused with 400; `PATCH` on a reference the caller's own tenant holds no row under is refused with 404, whatever the body; a metadata-only `PATCH` equal to the current record returns 204 with the unchanged `ETag` and does not bump `version`.
- E2E: a `Credential` whose caller holds no row under the reference carries neither `version` nor `updated_at` and a weak `ETag` that changes when the ancestor rotates and is refused in `If-Match`; a caller holding a `declared` row under a reference that resolves to an ancestor's value sees `inheritance: inherited`, `status: declared`, its own `version`, and a strong `ETag` that a guarded `PATCH {"value": …}` accepts.
- E2E: a record with `fallback: none` and no value resolves to 404 in its own tenant and, when `sharing: shared`, in its descendants too; the ancestor's record and value are untouched and still served outside that subtree; its `inheritance` reads `suppressed`; the same record scoped `tenant` instead leaves descendants inheriting normally; `PATCH {"fallback": "inherit"}` restores inheritance without touching the value; writing a value with `PATCH {"value": …}` makes the record `active` and `overridden`.
- E2E: suppressing an active own credential with `PATCH {"fallback": "none", "value": null}` under one `If-Match` makes the reference resolve to nothing at once — no intermediate read, at any point during or after the request, ever returns the ancestor's value.
- Contract: `Credential.status` never takes `provisioning` or `deprovisioning`; `fallback` appears only for the caller's own row, never for an ancestor's.
- Contract tests: the `Secret` schema contains exactly `reference`, `type`, `expires_at` and the value; `sharing`, `inheritance` and `status` appear in `Credential` only.
- E2E: a record made `declared` by `PATCH {"value": null}` returns 404 on its own secret **and** an ancestor's inherited value keeps resolving for descendants while the record stays value-less, provided its `fallback` is `inherit`.
- E2E: the periodic maintenance job leaves a `declared` record untouched across at least two of its runs.
- Audit: one record per value returned by both value-disclosing reads — the point read and the collection in value mode — and one per write whose body carried `value` (rotate or remove — tagged "value written" or "value removed") or metadata (tagged "record written"); no record claims a disclosure when the response carried none.
- E2E: a role with `read_secret` and not `read` receives 200 with the `Secret` envelope on `GET …/secret` and the canonical 404 on `GET /credentials/{ref}`; its value-mode `$filter` on the collection returns its in-scope items.
- E2E: a role with `write_secret` and no read action completes a `declared` record's value with `PATCH {"value": …}` `If-Match: "<etag-it-was-handed>"` — guarded against the exact `declared` generation it was given — or with `If-Match: *` when it holds no validator at all; it cannot perform a guarded write against a validator it has no way to obtain on its own, and it recovers a corrupted version by writing a fresh one with `If-Match: *`, under a new `value_id`, so that the row resolves again ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)).

## Backward Compatibility by Mode

Compatibility is recorded here, not optimized for (D1).

| Surface / mode | Compatible with what ships today | Detail |
|---|---|---|
| Value read | **no — moves and slims** | `GET /credstore/v1/secrets/{ref}` becomes `GET /credstore/v1/credentials/{ref}/secret`. Every consumer of a value changes its URL. The body becomes the `Secret` envelope — value, `reference`, `type`, `expires_at` — so `sharing`, `is_inherited` and `owner_tenant_id` no longer accompany a value; same `ETag`, same `no-store`. |
| Entity URL semantics | **no — inverts** | The item URL used to return the secret; it now returns the record. A client that keeps calling the old shape gets metadata, never a value, so the failure is a missing field rather than a silent disclosure. |
| Collection name | **no — renamed** | `secrets` → `credentials`. |
| `is_inherited` in the metadata | **no — replaced** | Superseded by `inheritance`, which distinguishes `own` from `overridden` where the boolean could not. A client reading the boolean reads one field's worth of a three-state answer. |
| `version`, `updated_at` on an inherited record | **no — removed** | Both described an ancestor's write activity. An inherited `Credential` carries a weak opaque `ETag` for change detection instead; own records keep both fields and the strong validator. |
| Remove value | n/a — new | `PATCH /credentials/{ref} {"value": null}` returns a record to `declared` without deleting it; the record's `fallback` decides whether the reference then inherits or resolves to nothing. |
| Suppression (`fallback`) | n/a — new | A record field, written with the record via `PUT` or `PATCH`; the earlier P2 sketch of a `POST`/`DELETE …/suppression` sub-resource is withdrawn. |
| `owner_tenant_id` in the metadata | **no — removed** | For an inherited credential this named an ancestor tenant the caller cannot learn by any other route, including ancestors above a barrier that a barrier-respecting traversal would never show it. Recoverable by an entitled caller through Account Management, one authorized read per level. Pending confirmation with the Tenant Resolver's owners that the upward chain stays unpublished. |
| Create | **no — moves** | `POST /credstore/v1/secrets` with a value in the body disappears; creation becomes `PUT /credentials/{ref}` + `If-None-Match: *`, one request, carrying the value in the same body — atomic under the [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md) write protocol: the value written under a fresh `value_id`, then the row created in one transaction. |
| Rotation | **no — moves** | `PUT /credstore/v1/secrets/{ref}` becomes `PATCH /credentials/{ref} {"value": …}` (or a full `PUT`); the `If-Match` contract itself is unchanged. |
| Metadata edit | n/a — new | Previously impossible without rewriting the value; now `PATCH /credentials/{ref}` with the changed fields only — no value need be resent. |
| List, bulk secret read (`$select=secret`), suppression | n/a — new | Nothing to break. Listing, including its value mode, is a documented non-goal today, so shipping it amends the PRD rather than breaking a contract. |
| SDK trait `CredStoreClientV1` | **no — reshaped** | The trait follows the merged write surface. `put(ref, record + value)` creates or replaces the whole credential; `patch(ref, merge)` applies a partial change to either or both; `get` returns a `Credential`, which has no `value` field, so every existing caller of `get` fails to compile rather than silently reading metadata; `get_secret` and `delete` are unchanged in shape. `list(ctx, query)` takes an OData query — filter, select, orderby, limit, cursor — and returns items whose `secret` is present only when `$select` names it, under the same value-mode rules as the REST collection; `read_secrets` is **removed**, not reshaped, since its one job is now `list` with `secret` selected. The method set is `get`, `get_secret`, `put`, `patch`, `list`, `delete` — six methods, none named `create` or `read_secrets`. `create` is removed, not merely renamed: `put` with `If-None-Match: *` **is** create, and a single-request atomic create is exactly what `put` now provides — a separate `create` method would name the same call twice. `put_secret` is removed; a value write is `patch` with a `value` key. In-repo consumers (OAGW, the keycloak-idp plugin) move to `get_secret` for reads and to `patch` for rotation; a consumer using the removed `read_secrets` moves to `list` with `$select` containing `secret`. |
| **Secret type ids** | **no — renamed** | `gts.cf.core.credstore.secret.v1~` becomes `gts.cf.core.credstore.credential.v1~`, and every derived id, `SECRET_RESOURCE_TYPE` and `GENERIC_TYPE_UUID_STR` follow. Custom types re-register under the new base; stored `secret_type_uuid` values change (a constant change while there are no production rows, a migration afterwards). |
| **Authorization model (resource type and actions)** | **no — replaced** | The resource type changes, so no shipped permission matches any new operation. The six actions are `list`, `read`, `write`, `delete` on the record and `read_secret`, `write_secret` on the value. Every policy is re-issued against `credential.v1~`. Writes are the one place action selection is not purely path-based: `PUT` always requires `write` and `write_secret` together; `PATCH` requires whichever of the two its body touches, derived from the keys present, evaluated before any side effect. |

**Why the authorization break is structural, not softened.** The tempting shortcut would be to accept the shipped `read` on the new surfaces so deployments roll forward untouched. It would leave a grant whose meaning is permanently ambiguous — a reviewer could not tell whether `read` was meant to include value disclosure — and since the premise of this ADR is that value disclosure is a separate privilege, an ambiguous grant defeats it. The type rename closes the question without a rule: shipped permissions name `secret.v1~`, the new operations authorize against `credential.v1~`, and the two never match. Re-granting is a release task with an explicit checklist: enumerate every role holding the shipped `read` or `write`, decide per role whether it needs the record, the catalogue, the value, or several, and issue the new grants.

**Rollout ordering that follows.** Grants first, endpoints second. If endpoints ship before grants are re-issued, reads fail closed — empty pages and 404s — which is safe but reads like an outage. The reverse order is safe and invisible. The roles table under Authentication and authorization is the checklist for the re-grant. HTTP consumers of the value migrate on their own schedule only if the old path is kept alive as a temporary alias; whether to provide that alias is a rollout decision, and this ADR does not require one.

## Impact Analysis by Domain

### Security

- **Assets and adversary.** The asset is the plaintext credential. The adversary of record is an over-granted or compromised principal inside the platform — a tenant user, an application token, a stolen bearer. Two goals are addressed: learning *which* credentials exist (reconnaissance) and obtaining *many* values in one action (exfiltration).
- **Integrity, not only confidentiality.** `write_secret` without any read action can replace a value its holder cannot see. For a signing secret or an OAuth client that is a takeover, not a denial: a `webhook-hmac` set to a known key forges every webhook, an `oauth2-client` set to one's own diverts the traffic. The write path is therefore audited like the read path (Operations), and an injector is granted per concrete type, never on a wildcard.
- **Attack surface delta.** Metadata addresses widen only the reconnaissance surface, deliberately, because the catalogue is a product requirement, and each is gated by its own action. **Two surfaces disclose a value** — `GET /credentials/{ref}/secret`, its own path, and `GET /credentials` in value mode, which shares its path with every catalogue read and is instead bounded by `read_secret` per item, the cap, and an audit selector on `$select` containing `secret`. Reconnaissance through value mode is capped by construction: `$select=secret` cannot enumerate beyond the cap, `limit`/`cursor` are refused rather than accepted, and an item the caller may not read is omitted rather than confirmed absent or present — a probing client learns nothing about a reference it did not already name or a type it is not granted. Value **writes**, by contrast, share the record's address with metadata writes — `PUT` and `PATCH /credentials/{ref}` — and are not separately throttleable by path; they are authorized and audited by inspecting the body instead, which is the accepted exception this ADR documents (see [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not)).
- **Reconnaissance containment.** Every refusal is the canonical 404, byte-identical to absence, including per item in a bulk response. Timing is not equalized; that is a pre-existing, accepted property of the gear (a resolved credential consults the PDP and the registry, a missing one does not — DESIGN §5.4).
- **Exfiltration containment.** No response can exceed the caller's own read scope, none is paginated, and value mode fails rather than truncates above its cap. The worst case for a compromised application token is "the credentials that token was already entitled to read", which is the same bound as with N point reads, reached faster.
- **Per-option risk** is analyzed inline below; the two material risks are values on the paginated collection (unbounded, walkable disclosure) and a header-driven opt-in (invisible to path-based policy and to access logs).
- **Data protection.** Values keep `no-store`, appear in no metadata schema, and are never logged. `value_fp` and `fp_key_id` never leave the gear on any surface.

### Authentication and authorization

- **AuthN:** unchanged. No new principal kinds, no session or token-format change.
- **AuthZ model:** six actions on the renamed resource type `gts.cf.core.credstore.credential.v1~…`: `list`, `read`, `write`, `delete` on the record; `read_secret`, `write_secret` on the value. Plain verbs for the entity follow the platform convention (`read`/`write`/`delete` in every other gear); the `_secret` suffix follows its compound-action pattern (`set_reaction`, `upload_attachment`) and names the sub-resource, never the entity. Value mode introduces **no new action** — it evaluates `read_secret` per item, so it can never grant more than N point reads.
- **Write authorization is derived from the body**, a deliberate exception to the path-based authorization every read address in this ADR follows: metadata keys present require `write`; the `value` key present (string or `null`) requires `write_secret`; both present require both, evaluated before any side effect. See [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not). A "declarer" role — `write` without `write_secret` — can no longer create records: `PUT` always requires both actions together, so a caller holding only `write` can edit the metadata of records that already exist (via `PATCH`) but can never bring a new reference into being.
- **Compatibility of the model:** deliberately not preserved; see the table above. The break is structural: a shipped permission on `secret.v1~` matches no operation on `credential.v1~`.
- **No escalation path via metadata.** The type is immutable after create (`fr-override-type-consistency`) and is the only scope axis a grant names, so no metadata write can move a credential from one application's grant into another's — the escalation path a mutable `category` used to open, including through the scoped value-mode selector, does not exist. Retargeting which application may read a value means declaring a new subtype and reissuing the credential under it, not editing a field.
- **`write` is the most privileged metadata action.** It moves `sharing`, which publishes a secret to every descendant, and it owns `fallback`, which is suppression. A role that may edit an expiry may therefore also publish; there is no finer split, on purpose — six atoms express every role below, and a seventh would buy a separation nobody has asked for. Policy owners treat `write` as a grant on policy, not on labels.
- **Type-scoped grants are now the only scoping mechanism, with one caveat.** With `category` gone, the type is the entire purpose axis: an application granted `read_secret` on a subtype receives exactly the credentials of that subtype, and a consuming service that needs "its" credentials declares its own derived type rather than filing records under a label. A permission's resource type accepts GTS wildcard patterns (`…credential.v1~cf.core.credstore.basic_auth.v1~*`), so "may read the value of every basic-auth credential" needs no attribute predicate at all. The caveat is unchanged: registering a custom type needs no credstore release, and every subtype registered later under a wildcard is granted the moment it exists — the dangerous direction for a secret store — so grants on secret types name concrete types or an explicit set, and a wildcard on the base type is an operator's tool, not an application's. A bare shape type (`api_key`, `generic`) cannot be granted per purpose this way; a purpose needs its own subtype.
- **Implications between the actions**, stated so a policy author does not grant one believing it withholds another. `list` on a scope discloses every record `read` would, since the list item and the point record share one schema. `read_secret` on an item discloses that item's usage envelope and, through the filtered bulk read, the names of every readable item in scope. Separate grants therefore work downward — metadata without value, one record without the catalogue — never upward.
- **The roles the six actions actually form.** Six grant sets in four scenario families; this table is the re-grant checklist the rollout section calls for, and the actors are the PRD's.

| Scenario | Role | Grant | PRD actor |
|---|---|---|---|
| Runtime consumption | Consumer | `read_secret`, with `read` alongside so the record address does not 404 for a caller that may read the value | `integration-app`, `oagw`, `platform-gear` |
| Runtime consumption | Self-rotating consumer | `read_secret` + `write_secret`; the `ETag` arrives with the value, so no `read` is needed; rotates with `PATCH {"value": …}` | `self-rotating-app` |
| Administration without plaintext | Value-blind configurator | `list` + `read` + `write` + `write_secret` + `delete`; never `read_secret` — creates with a single `PUT`, edits metadata with `PATCH`, rotates with `PATCH`, all without ever reading a value | `integrations-admin` |
| Administration without plaintext | Catalogue and audit | `list` + `read` | `catalogue-auditor` |
| Machine provisioning | Injector | `write_secret` alone: writes and rotates values of records someone else created, via `PATCH {"value": …}` `If-Match: "<etag-it-was-handed>"` when guarded, or `If-Match: *` when it holds no validator; re-writes a value under a fresh version if a backend entry is found corrupted; `write` as well only if it also edits metadata | `provisioner` |
| Full control | Tenant admin, break-glass operator | all six; the operator differs by scope, not by action set | `tenant-admin` |

  Two of the six define no role on their own: `read` is the floor the others imply, and `delete` rides with `write` in every role above. They stay separate atoms for audit and for downward grants, not because any role needs them alone.

### Integration and contract

- **Breaking changes:** yes, and enumerated above. The value read and the collection name move; in-process SDK consumers break at compile time rather than silently — `get` returns a `Credential` with no `value` field, `create` and `read_secrets` are gone — and move to `get_secret`, `put`, `patch` and `list`.
- **Contract additions:** the metadata collection (with `$select`-driven value mode for the bulk case), the metadata point read, `PUT` and `PATCH` on the record (metadata mutation, value mutation, or both in one request), the secret read sub-resource, and suppression (`fallback`). There is no value *write* address any more — `PUT …/secret` and `DELETE …/secret` are removed, not merely deprecated.
- **Consumer action required:** HTTP consumers of a value update one URL; consumers wanting the new capabilities implement them. Bulk clients must treat a short `items` list as partial success, since a refused item is omitted rather than reported.
- **Prerequisites:** amending the PRD non-goal on listing, and the companion ADR for the collection read under the no-projection PEP contract (DESIGN §4.4).
- **Deprecation:** nothing is deprecated. If a temporary alias for the old value URL is provided, its removal date belongs to the rollout plan, not to this ADR.

### Operations

- **Rate limiting.** Gateway route policies match paths. The point value read keeps its own path and its own stricter rule; the bulk read does not — it shares `GET /credentials` with every catalogue read, so it cannot be throttled separately from a catalogue page by path alone, only by inspecting `$select`, which the gateway does not do today (see [Revisit Triggers](#revisit-triggers)). Value **writes** have the same limitation for the same reason they always did: both travel through `PUT`/`PATCH /credentials/{ref}`; this is an accepted cost of the merged write address.
- **Audit.** One record per value returned, on both value-disclosing reads: `GET /credentials/{ref}/secret`, named by path, and `GET /credentials` in value mode, selected by `$select` containing `secret` rather than by path. One record per write, tagged from the body rather than the path: "record written" when the body carried metadata, "value written" when it carried a non-null `value`, "value removed" when it carried `value: null` — a `PUT` that carries both produces both tags. `write_secret` is an integrity privilege whose blast radius equals disclosure for some types (Security), so who rotated or removed what, and when, is recorded like who read what. Metadata reads are not audited per record; the collection read may be sampled if volume warrants.
- **Metrics.** Value-mode request rate and result size distribution, per-item refusal counts by cause, cap rejections. Existing read-outcome and fence metrics extend unchanged.
- **Runbook.** A spike of `TOO_MANY_MATCHES` or of per-item refusals is the operator-visible signal of a misbehaving or probing client.
- **No new infrastructure.**

### Performance

- **Metadata reads** are strictly cheaper than today's read: one indexed resolution, no backend call, no fence verification.
- **Point value read** is unchanged in cost.
- **Bulk** performs up to the cap backend reads and fence verifications, plus one PDP evaluation per distinct type present in the result. It carries its own SLO and is excluded from the point-read latency budget.
- **Why the cap is the control.** Without it, request cost and disclosure blast radius grow together; one number bounds both.

### Reliability

- **Partial failure is normal** in bulk: an item may fail to resolve, be refused, be suppressed, or fail its fingerprint check, and the rest are still served.
- **Every write failure costs at most garbage, never a torn state.** `PUT` and a value-bearing `PATCH` follow the [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md) write protocol: the backend is written under a fresh `value_id` before any row changes, so a failure before the commit leaves an orphan the writer's own cleanup or the periodic maintenance job removes, and a failure after the commit leaves nothing pointing at stale bytes; a metadata-only `PATCH` is one row update; `PATCH {"value": null}` and `DELETE` are each one transaction, with the backend touched only after commit. No failure mode ever serves a wrong value, closes a read that should succeed, or wedges a name.
- **Fence behaviour preserved:** a mismatch still fails closed, now scoped to one item.

### Data

- **Schema changes are three, and all are named elsewhere in this ADR.** The fourth `status` value `declared` (the value-less record); the type-id rename, which changes every stored `secret_type_uuid` ("Why `credentials` and not `secrets`"); and the `fallback` column, `fallback SMALLINT NOT NULL DEFAULT 1 CHECK (fallback IN (1, 2))` (Suppression above), following the same SMALLINT-code-with-CHECK convention as `status` and `sharing`. No further new column originates here.
- **No new personal data.** References are operator-chosen labels; values stay opaque bytes to the gear.
- **Retention and residency:** unchanged; nothing new is persisted.

### Maintainability and evolution

- A future **exposure** class is a new address, not a new flag, so every response keeps one shape and every disclosure policy stays path-addressable — the writes above are the documented exception, not a precedent for reads.
- If a legitimate caller outgrows the cap, the answer is to raise it deliberately — a reviewed configuration change with an audit consequence — not to add pagination to a value read.
- "Metadata carries no value" is guaranteed by the resource layout and pinned by contract tests, so it survives refactoring instead of relying on reviewer vigilance.

### Compliance

- Supports `nfr-confidentiality`: values stay out of caches, logs and metadata schemas, and every disclosure is attributable to a subject and an operation.
- Separating "may see that a credential exists" from "may read it" is what makes least privilege expressible for the integration-administrator role, which is the compliance-relevant outcome.

### Not applicable

- **Usability:** the gear exposes no end-user interface; the administrator experience lives in the consuming console, and this ADR only fixes the API it consumes.
- **Session management:** the gear holds no sessions.
- **Data migration:** see Data — the type-id rename changes every stored `secret_type_uuid`, a constant change while there are no production rows and a migration afterwards; `declared` and `fallback` are additive, with defaults.
- **Penetration testing as a gate:** the security confirmation at ADR level is the contract and e2e invariants listed above; scheduled testing is a platform activity outside this decision.

## Revisit Triggers

- A caller needs more credentials per request than the cap can serve, or needs a selector richer than `eq`/`in` over type and reference — prefix or range matching on `reference` is the likely first ask, and it is out of the grammar above on purpose, because a prefix scan over names is the enumeration primitive this ADR exists to withhold.
- The gateway gains query-aware or header-aware route policies, which would remove the main argument against a flag-based opt-in (B2, B3).
- Audit starts deriving "value read" from response bodies rather than from the operation, which would remove the second argument.
- The Tenant Resolver grows an HTTP API that exposes the ancestor chain to callers. The `owner_tenant_id` removal rests on that chain being unobtainable outside the process; once it is obtainable, the field costs nothing and can come back.
- A value-mode request needs `$orderby` or pagination, which this ADR refuses on purpose (D6) — B5's objections, not B6's, would then be back in scope.
- The gateway needs to throttle value reads on the collection separately from catalogue reads, which would argue for restoring a dedicated bulk address.
- Tenants want to block an inherited credential without a value of their own often enough that a value-less create (`PUT` with `value: null`) is wanted; today that is a two-request path (`PUT` with some value, then `PATCH` it away), accepted as a cost of C2 — see [Two write verbs on one resource: PUT and PATCH, no POST](#two-write-verbs-on-one-resource-put-and-patch-no-post).
- The gateway needs to throttle value writes separately from metadata writes, which would argue for restoring a value write address — reopening C1, or some narrower form of it.
- False conflicts between metadata edits and value rotations become a real annoyance; the fix is an independent `ETag` per sub-resource instead of one shared `version`.
- The gear becomes the successor of `credentials-storage`; the shared noun then has to become a shared contract, and the compatibility facade is a decision of its own.
- A tenant needs to stop serving its own active credential while keeping its value in place. That would be a fourth state — value present, not served — which this ADR excludes on purpose so that `status` and `fallback` stay orthogonal; suppression with the value removed (`PATCH {"fallback": "none", "value": null}`) is the answer offered instead.

## Pros and Cons of the Options

### A1: `secrets` collection, value on the item, metadata at `meta`

- Good: the value read keeps its current URL, so HTTP consumers do not move.
- Bad: the entity URL returns the sensitive payload and the safe view is the exception, which inverts the default one wants for credentials.
- Bad: the list item and the point item have different schemas — the collection omits a field the item returns — so clients cannot reuse one type (D8).
- Bad: forces the sentence "`GET /secrets` returns no secrets" into the documentation, and the `meta` suffix has to be explained on every review.

### A2: `credentials` collection, metadata on the item, value at `secret` (CHOSEN)

- Good: metadata surfaces cannot carry a value because the value is not part of that resource; the invariant is structural.
- Good: one schema for the list item and the point record (D8); `ETag` naturally available to metadata readers (D4).
- Good: the point read has its own path, so gateway policy, rate limits and audit selectors need no body inspection for it (D5); writes have their own path too, though the action they require is derived from the body (C2). Not the collection, though: `GET /credentials` in value mode shares its path with every catalogue read, so it is bounded by cap, per-item authorization and an audit selector on `$select` instead — see Consequences.
- Good: vocabulary matches the domain and converges with the existing `credentials-storage` service, which matters if the gear becomes its successor.
- Good: with the base type renamed alongside, the type, the collection, the schemas, the actions and the SDK share one noun for the entity and one for the payload; no layer has to translate.
- Bad: renames the collection and moves the value read; every HTTP consumer of a value changes.

### A3: `secrets` collection, value at `value`

- Good: the same structural benefit as A2 with a smaller rename — the collection name survives.
- Bad: muddled vocabulary, in which "secret" names the record and "value" names the secret; every reader has to hold that inversion in mind.
- Bad: keeps a name that will read oddly next to `credentials-storage` if the two converge.
- Bad: the entity's noun is the payload's noun, so `GET /secrets/{ref}` promises a secret and returns none of it; no schema name fully removes that reading.

### B1: value only at its own address (CHOSEN)

- Good: privileges, audit, rate limits and cache policy align with paths; fixed schema per address.
- Bad: more endpoints; bulk clients handle partial results, since a refused item is omitted.

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
- Bad: collapses `list` and `read_secret` into a single effective privilege (D2).
- Bad: pagination interacts badly with per-item authorization and with the fence; a page would mix served values, fence-failed items and authorization holes, and a cursor would let a client resume the walk.
- Rejected. The legitimate need behind it — "all my SMTP secrets" — is served by B6 below, which keeps the scoped selector but refuses pagination and caps the result.

### B6: values on the collection under `$select`, non-paginated and capped (CHOSEN for bulk)

- Good: one round-trip for "everything I may read", reusing the collection's own `$filter` and the platform's own projection mechanism (`OperationBuilderODataExt::with_odata_select`) rather than a bespoke selector grammar or a second address.
- Good: every rule that sank B5 is answered directly, not merely asserted: `limit`/`cursor` are rejected outright in value mode (D6), the result is capped and fails closed above it (`TOO_MANY_MATCHES`), each item is authorized under `read_secret` per distinct type, and a reference the filter found but the caller may not read is omitted rather than listed — none of B5's "walkable catalogue dump" shape survives.
- Good: `$select` is sparse projection of a single schema (D8), not a second response shape, so the objection that sank B5 (`list` and `read_secret` collapsing into one privilege, D2) does not apply: value mode still evaluates `read_secret`, metadata mode still evaluates `list`, and no request evaluates both.
- Bad, and stated plainly: the path no longer distinguishes disclosure on the collection. `GET /credentials` without `secret` in `$select` is a catalogue read; the same path with `secret` selected is a value read. D5's "throttleable and auditable by route" still holds for the point read and for writes, not for the collection — value-mode requests are told apart by inspecting the query, bounded by the cap and per-item authorization, and audited on `$select` containing `secret` rather than on the path alone.
- Rejected as the shape for the point read — B1 keeps that address, unchanged — chosen as the shape for the bulk read, replacing the withdrawn `POST /credentials:read-secrets`.

### C1: value sub-resource with its own `PUT`/`DELETE`

This is what this ADR chose in its first version, and what the rest of this document described until this revision replaced it with C2.

- Good: symmetric with the read side (B1) — one address per privilege, one schema per address, no body inspection to authorize a write.
- Good: each `PUT` is a true whole-resource replace of a resource that actually has only the field it manages, so "fields absent reset to defaults" never has to reckon with a mixed metadata-and-value body.
- Bad: creating a credential is two requests with no atomicity between them — `PUT` the record, then `PUT` the value — and a client that stops after the first leaves a value-less record behind as an unintended failure mode, indistinguishable from a deliberate one.
- Bad: the value sub-resource has no representation of its own for a precondition to be evaluated against — a value that has never been written has nothing RFC 9110 would call a current representation — so `If-Match` and `If-None-Match` on it have to be redefined against the **record's** validator, a stated deviation rather than a direct application of the standard; `If-None-Match: *` in particular has to be repurposed to mean "only while no value is set" instead of its RFC 9110 meaning.
- Bad: suppressing a currently-active own credential without a window between requests needs an explicit ordering rule — arm `fallback: none` on the record, only then delete the value — because the two changes are necessarily two requests, and getting the order wrong briefly serves an ancestor's value to a tenant that meant to block it.
- Rejected. Every one of these costs is a consequence of one thing: the value's own `PUT`/`DELETE` cannot see or touch the record in the same request, so anything that legitimately needs to change both — create, and suppress-while-active — needs two.

### C2: `PUT` (full replace) and `PATCH` (merge) on the record (CHOSEN)

- Good: creation is atomic again — one `PUT`, one write ([ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md)), record and value together; there is no window in which a value-less record exists by accident.
- Good: suppressing a currently-active own credential is atomic too — one `PATCH` carrying both `fallback` and `value: null`; no ordering rule is needed because there is nothing left to order.
- Good: one resource, one validator, one `ETag` — no second representation to define a precondition against, so no deviation from RFC 9110 needs stating.
- Good: `PATCH`'s merge semantics are RFC 7396 as published, not bespoke, which answers the objection an earlier draft of this ADR raised against `PATCH` in general; and `PATCH` is exactly what lets a value-blind administrator edit metadata on a resource that also holds a value, without resending or destroying it — the case a merged `PUT` alone could never serve.
- Bad: a write's required action(s) are derived from its body rather than its address, the one place in this ADR that path-based authorization is deliberately given up (see [Why the value has its own read address, and why writes do not](#why-the-value-has-its-own-read-address-and-why-writes-do-not)).
- Bad: creation now needs both `write` and `write_secret` together, since a `PUT` always carries a `value`; a `write`-only "declarer" role can edit an existing record's metadata but can no longer create one.
- Bad: no single-request value-less create: a tenant that wants to suppress an inherited credential without ever holding a value of its own still needs two requests — `PUT` with some value, then `PATCH` it away — because `PUT` always carries a `value`. See [Revisit Triggers](#revisit-triggers).
- Bad: value writes can no longer be rate-limited or audited by path separately from metadata writes, since both now share `PUT`/`PATCH /credentials/{ref}`; the gateway throttles the address, not the body.

## Traceability

- Requirements (shipped): `cpt-cf-credstore-fr-get-secret`, `cpt-cf-credstore-fr-put-secret`, `cpt-cf-credstore-fr-delete-secret`, `cpt-cf-credstore-fr-optimistic-concurrency`, `cpt-cf-credstore-fr-service-retrieve`, `cpt-cf-credstore-nfr-confidentiality`, `cpt-cf-credstore-nfr-tenant-isolation`.
- Requirements (proposed by the same project, added to the PRD in §5.8): `-fr-credential-record`, `-fr-list-credentials`, `-fr-get-credential`, `-fr-write-credential-record`, `-fr-read-secret`, `-fr-write-secret`, `-fr-bulk-read-secrets`, `-fr-authz-action-split`, `-fr-inheritance-status`, `-fr-override-type-consistency`; `-fr-suppression`.
- Builds on [ADR-0003](0003-cpt-cf-credstore-adr-value-fingerprint-fence.md): the fence runs per returned value, including per item in a bulk read.
- Builds on [ADR-0006](0006-cpt-cf-credstore-adr-immutable-value-versions.md) for the write protocol below the surface.
- Depends on the companion ADR for the collection read under the no-projection PEP contract ([ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md)). The PRD non-goal it needed lifted is already narrowed in this change: listing credential records is in scope, and only full-text search over names or values remains a non-goal.
- Pagination and filtering of the credential collection follow `guidelines/DNA/REST/PAGINATION.md`; value mode deliberately opts out of the cursor machinery while keeping the platform `$filter` and `$select`. `$select` for field projection follows `guidelines/DNA/REST/API.md`'s general recommendation, declared through `OperationBuilderODataExt::with_odata_select` (`libs/toolkit/src/api/operation_builder.rs`); credstore is its first consumer, and value mode is the first place selecting a field changes which PDP action a request needs.
- Answers the open questions recorded in `PRD.md:717` / `DESIGN.md:790` (batch retrieval) and the value-exposure half of `PRD.md:720` / `DESIGN.md:793` (metadata list vs anti-enumeration).
- `$`-prefixed parameters must be declared through `OperationBuilderODataExt` (`libs/toolkit/src/api/operation_builder.rs:447`, `with_odata_filter`/`with_odata_select`) to satisfy the external `cargo-gears` lint `de0802_use_odata_ext`; `limit` and `cursor` have no helper and are declared manually where they apply.
