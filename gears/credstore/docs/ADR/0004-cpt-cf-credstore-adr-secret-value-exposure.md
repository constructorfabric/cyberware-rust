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
- [Decision Outcome](#decision-outcome)
  - [Resulting surface](#resulting-surface)
  - [Why one address per thing, in three arguments](#why-one-address-per-thing-in-three-arguments)
  - [Why `credentials` and not `secrets`](#why-credentials-and-not-secrets)
  - [Why the path key stays a reference, not a UUID](#why-the-path-key-stays-a-reference-not-a-uuid)
  - [One write verb per resource: no `PATCH`, no `POST`](#one-write-verb-per-resource-no-patch-no-post)
  - [Consequence: no single-request create of record and value](#consequence-no-single-request-create-of-record-and-value)
  - [What a response says about tenants above: `owner_tenant_id` is dropped](#what-a-response-says-about-tenants-above-owner_tenant_id-is-dropped)
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

At a glance, eight addresses replacing the four above:

| Method and path | Returns | PDP action |
|---|---|---|
| `GET /credstore/v1/credentials` | the list of records, no values, paginated | `list` |
| `GET /credstore/v1/credentials/{ref}` | one record (metadata), carries the `ETag` | `read` |
| `PUT /credstore/v1/credentials/{ref}` | create or replace the record | `write` |
| `DELETE /credstore/v1/credentials/{ref}` | delete the record | `delete` |
| `GET /credstore/v1/credentials/{ref}/secret` | the value | `read_secret` |
| `PUT /credstore/v1/credentials/{ref}/secret` | set or rotate the value | `write_secret` |
| `DELETE /credstore/v1/credentials/{ref}/secret` | clear the value; the record stays, `declared` | `write_secret` |
| `POST /credstore/v1/credentials:read-secrets` | values for a bounded selection, in one request | `read_secret`, per item |

Read against the four shipped addresses in the Context above, three things changed: the collection is named for the record rather than the payload, the value moved to a sub-resource of its own, and the three actions on the shipped `secret.v1~` type become six on the renamed `credential.v1~` type — plain verbs for the record, `_secret`-suffixed verbs for the value — with every address answering to exactly one of them. The same table with the headers, preconditions and cache rules each address carries:

| Address | Returns | PDP action | Notes |
|---|---|---|---|
| `GET /credstore/v1/credentials` | credential records (metadata), paginated | `list` | value-free by construction; `Cache-Control: no-store`, because the body varies by tenant and subject; OData filter/order per `guidelines/DNA/REST/PAGINATION.md` |
| `GET /credstore/v1/credentials/{ref}` | one `Credential` (metadata) | `read` | carries the `ETag` — the CAS validator source (D4): a strong `"<id>.<version>"` for the tenant's own row, a weak opaque `W/"…"` for an inherited one (see "What a response says about tenants above"); `Cache-Control: no-store` |
| `PUT /credstore/v1/credentials/{ref}` | — (201 on create with `ETag`, 204 on replace) | `write` | create-or-replace of the **record only**: sharing, type, category, expiry. Preconditions carry the intent: `If-None-Match: *` create-only, `If-Match: "<etag>"` guarded replace, `If-Match: *` last-writer-wins. A body equal to the current record is a no-op: 204, same `ETag`, no version bump |
| `DELETE /credstore/v1/credentials/{ref}` | — (204) | `delete` | `If-Match` required; releases the reference |
| `GET /credstore/v1/credentials/{ref}/secret` | one `Secret`: the value plus `reference`, `type`, `expires_at` | `read_secret` | `Cache-Control: no-store`; audited; carries the record's `ETag` so a self-rotating caller needs no second read |
| `PUT /credstore/v1/credentials/{ref}/secret` | — (204) | `write_secret` | set or rotate; a precondition is required and evaluated against the **record's** validator (see below); `If-None-Match: *` means "only while no value is set"; never creates a record; always writes and always bumps the version, even for identical bytes |
| `DELETE /credstore/v1/credentials/{ref}/secret` | — (204) | `write_secret` | clears the value and returns the record to `declared`: metadata, category and sharing survive, the reference stays reserved, and — as for any `declared` record — the reference stops resolving and stops shadowing an inherited value; `If-Match` required |
| `POST /credstore/v1/credentials:read-secrets` | values for a bounded selection | `read_secret`, per item | bulk; see below |

### Why one address per thing, in three arguments

The table above spends eight addresses where four would do. That is the cost, and each of the three reasons below is on its own sufficient to pay it.

**One address, one schema.** A response body is fixed by the URL, not by who asked or by what was sent. A generated client gets one type per address; an OpenAPI reader sees one shape; a reviewer checking "can this path emit a secret" reads the schema rather than the handler. The alternative — one address whose body sometimes carries a value — makes the schema a function of the caller's grants, which is untypeable, and turns "does this leak" into a question about runtime state instead of about a document.

**One address, one PDP action.** Route policy at the gateway, rate limits and audit selectors all match on paths, and the audit record has to say "this subject read this value" as a property of the operation rather than of the payload. Collapse the value into the record's address and the required privilege becomes a function of the request body: the server would have to inspect what was sent to decide which action to authorize, and the gateway could no longer throttle value reads separately at all. Deriving a privilege from a payload is the shape of an authorization bug, not an optimization.

**One address, one intent — and this is the decisive one.** `PUT` is a whole-value replace: fields absent from the body reset to their defaults. Put the value and the metadata behind one `PUT` and every write has to answer a question with no good answer.

Change a category and omit the value: does the value get cleared, or preserved? Clear it and an innocent metadata edit destroys a credential. Preserve it and `PUT` is no longer a whole replace, so the same body means "reset" for one field and "leave alone" for another — exactly the ambiguity that made us refuse `PATCH`. The way out — "then send the value back" — is closed to the one caller this ADR exists for. The integration administrator holds `write` and `read` and not `read_secret`: it can read every field of the record except the value, so it can never construct a body that preserves it. Under a merged `PUT` that administrator either cannot change a category at all or destroys the credential by changing it. The value-blind metadata edit — the whole point of the split — is unexpressible.

Rotation suffers too, though less. The rotator does hold `read` — D4 has it read the record for the `ETag` before a guarded write — so it *can* send the metadata back verbatim; but every rotation becomes a read-modify-write of fields it never meant to touch. That bites under the `If-Match: *` this ADR keeps for provisioning and healing flows: a routine rotation then overwrites a concurrent `sharing` or expiry change with the stale copy it read a moment earlier. With the value at its own address the same `If-Match: *` rotation cannot touch a metadata field, because none is in the body.

Two addresses answer both without a rule to remember: each `PUT` carries the complete state of its own resource, absence means default, and the privilege is whatever that address requires. The record write cannot touch the value because the value is not in that resource, and the value write cannot reset the metadata for the same reason.

Suppression (the "disabled here" tombstone, if adopted) attaches to the record: `POST` / `DELETE /credstore/v1/credentials/{ref}/suppression`.

**Two representations, and what each carries.** `Credential` is the record: `reference`, `type`, `category`, `sharing`, `status` (`declared` or `active`, so a console can show a half-finished create), `expires_at`, `inheritance` (`own` / `inherited` / `overridden`), and — for the caller's own record only — `version` and `updated_at`. `Secret` is the value with exactly what is needed to use it: `reference`, `type` (a consumer must know whether it is parsing a `basic_auth` object or an `api_key` string), `expires_at` (when to come back), and the value itself; the record's `ETag` travels in the header so that a caller which reads and rotates its own credential never needs the record address. Nothing administrative rides along with a value: `sharing`, `category`, `inheritance`, `status` stay behind `read`. That is what keeps `read_secret` and `read` two different privileges instead of one nested inside the other, and it is why the question "should the value address require both actions" never arises — each address discloses one representation, and one action covers it.

### Why `credentials` and not `secrets`

Under A2 the collection name has to describe the record, not the payload. "Credential" is the record — a named, typed, tenant-scoped entry with sharing, version and expiry; "secret" is the sensitive value it holds. That vocabulary is the one this project already uses when talking about the two read classes, and it removes the sentence A1 forces you to write: *"`GET /secrets` returns no secrets."*

It also converges with the platform: the Constructor service that already implements a version of this domain is `credentials-storage`, whose collections are `credential-definitions` and `credentials`. If the gear becomes its successor, a shared noun makes the migration and any compatibility facade legible instead of a translation exercise.

**The GTS type follows the noun.** A permission is a pair of resource type and action, and the resource type is the GTS type of the entity. Leaving it at `gts.cf.core.credstore.secret.v1~` while the collection says `credentials` would put two nouns for one thing into every policy — "on resource *secret*, action *read the credential*" — and would leave the type naming the payload rather than the entity it types: a type's traits (`allow_sharing`, `expirable`, `value_schema`) describe the credential as a whole, not its bytes. So the base type is renamed to `gts.cf.core.credstore.credential.v1~`, and every derived type follows (`…credential.v1~cf.core.credstore.basic_auth.v1~`). `SECRET_RESOURCE_TYPE`, the seeded catalog and `GENERIC_TYPE_UUID_STR` change with it; the stored `secret_type_uuid` is the v5 UUID of the type id, so every stored value changes too. Nothing outside the credstore crates references the old id. While the gear has no production rows this is a constant change; afterwards it is a data migration and a re-registration of every custom type, which is why the rename is decided here and not later.

One consequence is worth stating because it removes a rule this ADR would otherwise need. Shipped permissions target `secret.v1~`; the new surface authorizes against `credential.v1~`. No shipped grant can match any new operation, so the authorization break below is structural rather than a convention ("the old `read` is not a synonym") that reviewers would have to police.

**"Credential" is used in its broad sense**: evidence of identity or authority — passwords, API keys, tokens, OAuth clients, certificates, webhook signing secrets. Nearly every type in the catalog is one. `generic` is the documented exception for opaque material, kept because a store that types its entries still needs a type for the untyped, not because the noun fits it.

With the type renamed, every layer says the same two words: the type, the `credentials` collection, the `Credential` and `Secret` schemas, the `read`/`read_secret` actions, and the SDK's `get`/`get_secret`.

### Why the path key stays a reference, not a UUID

`{ref}` remains the caller-chosen `SecretRef` (`[A-Za-z0-9_-]+`), not the row UUID. The reference is the stable name a consumer hard-codes; the row id is a *generation* id, minted fresh whenever a credential is deleted and recreated, and it is deliberately kept out of response bodies so that CAS validators have exactly one source (the `ETag`). A UUID in the path would force consumers to resolve name → id on every call and would make a recreated credential a different URL, breaking the one property applications rely on.

### One write verb per resource: no `PATCH`, no `POST`

Each resource has exactly one write verb, `PUT`, and the intent of a write is carried by RFC 7232 preconditions rather than by the verb:

| Intent | Request |
|---|---|
| create the record, fail if it exists | `PUT /credentials/{ref}` + `If-None-Match: *` → 201, or 409 if the caller's tenant already holds a record under that reference |
| replace the record, only if unchanged since I read it | `PUT /credentials/{ref}` + `If-Match: "<id>.<version>"` → 204, or 409 |
| replace the record, last writer wins | `PUT /credentials/{ref}` + `If-Match: *` → 204 |
| set the value only if none is set yet | `PUT /credentials/{ref}/secret` + `If-None-Match: *` → 204, or 409 if the record already holds a value |
| set or rotate the value | `PUT /credentials/{ref}/secret` + `If-Match: "<id>.<version>"` or `If-Match: *` → 204, or 409 |
| clear the value, keep the record | `DELETE /credentials/{ref}/secret` + `If-Match` → 204; the record is `declared` again |

**"If it exists" is judged against the caller's own tenant.** `GET /credentials/{ref}` may well return 200 for a reference the caller has never declared — an ancestor's `shared` credential is a current representation of that URL — and RFC 9110 read literally would then fail `If-None-Match: *`. That reading would make the override flow unexpressible: the case in which a tenant declares its own record is precisely the case in which the reference already resolves to an ancestor's. So the create-only precondition asks "does *my* tenant hold a row under this reference", inherited representations do not count, and the deviation is stated here rather than discovered in a test.

**One status code for every failed precondition: 409.** RFC 9110 offers 412 for a failed `If-None-Match` and the platform maps a version conflict (`OPTIMISTIC_LOCK_FAILURE`) to 409; using both would make "your precondition did not hold" two different codes depending on which precondition it was. The platform mapping wins, and the reason code in the body says which precondition failed.

`PATCH` is deliberately absent. A partial-update verb needs its own merge semantics and its own conflict rules on top of the ones `PUT` already has, and it invites the pattern where a client mutates one field without ever holding the whole record — which is precisely how a `sharing` change gets made without noticing the expiry it left behind. `PUT` on the record is a whole-value replace: fields absent from the body are reset to their defaults, exactly as today's value `PUT` already clears an omitted expiry.

`POST` on the collection is absent too, because `PUT` with `If-None-Match: *` already expresses create-only, keeps the write idempotent, and puts the name in the URL where it belongs rather than in the body. The one thing `POST` bought — creating the record and its value in a single atomic request — is given up on purpose; see the next section.

The record's `type` remains immutable: a `PUT` whose body names a different type is rejected rather than applied, so "whole-value replace" covers the mutable metadata only.

**Versioning across the two resources.** One monotonic `version` lives on the record, and both writes bump it. A validator read from the record therefore guards value writes as well, and a concurrent rotation invalidates a pending metadata edit. That is conservative — a metadata edit can be rejected because someone rotated the value — but it keeps one validator, one counter and one `ETag` in the system. Independent per-sub-resource `ETag`s would be more precise and are the obvious extension if the false conflicts ever hurt.

**A record write that changes nothing bumps nothing.** `PUT /credentials/{ref}` with a body equal to the current record — after normalization, expiry included — returns 204 with the unchanged `ETag`, leaves `version` and `updated_at` where they were, and writes no row. A retried or idempotently re-sent record write therefore cannot invalidate a concurrent writer's validator or fake a change in the catalogue. Validation still runs first: a re-sent record naming a category that has since been deprecated is refused, not waved through as a no-op.

**A value write never takes that shortcut.** `PUT /credentials/{ref}/secret` writes the backend and bumps the version every time, identical bytes included, for two reasons that are each sufficient. First, deciding "unchanged" would mean comparing the submitted value against the stored fingerprint, and the outcome would be observable — through the version, the `ETag`, or `updated_at`, all readable under `read` — by a caller holding `write_secret` and not `read_secret`. That caller could then submit guesses and watch whether the version moved: an equality oracle on a value it is not allowed to read, which is exactly the disclosure class this ADR exists to close. Skipping the backend write would add a timing channel on top. Second, the `If-Match: *` re-write of the *same* value is the recovery path for a fence-poisoned row (ADR-0003): the row's fingerprint no longer matches what the backend holds, the caller re-injects the value it knows to be right, and a fingerprint-based no-op check would see "unchanged" and leave the poison in place. The value path stays oblivious to content equality.

### Consequence: no single-request create of record and value

**A new credential now takes two requests**, and this is an accepted cost of the resource split:

```
PUT /credstore/v1/credentials/smtp-default        If-None-Match: *   → 201
PUT /credstore/v1/credentials/smtp-default/secret If-Match: *        → 204
```

**What a value-write precondition is compared against.** The value sub-resource has no validator of its own, because a record and its value share one `version` — a value write bumps it exactly as a record write does. A precondition on `PUT …/{ref}/secret` is therefore evaluated against the **record's** validator, the same `"<id>.<version>"` the record's `ETag` carries.

This is a deliberate deviation from RFC 9110, which evaluates a precondition against the target resource's own current representation, and it is stated rather than left to be inferred: read the other way, `If-Match` on a value that has never been written would be evaluated against an absent representation and could not succeed, which would make the first of the two writes above unexpressible. The record's `ETag` exists from the moment the record is created — which is why record creation returns it, rather than making the client insert a metadata `GET` between the two calls — so a guarded first value write is available immediately, and `If-Match: *` on the sub-resource means "the record must exist". A value write against a reference with no record is a 404, not a create: the record is the thing that gets created, and only by its own `PUT`.

**`If-None-Match: *` on the value means "only while no value is set".** Evaluated against the record's representation it could never succeed — the record always exists for a legal value write — so it is given the one meaning that is useful here: the write succeeds while the record is `declared` and is refused with 409 once a value is present. That is the set-once a provisioning pipeline wants ("initialize if empty, never overwrite a rotation someone else made"), and it is the only precondition a writer holding no read action at all can use meaningfully besides `If-Match: *`, since it can obtain no validator.

Between them the record exists **without a value**, so that state has to be legal and defined rather than transient:

- **A value-less record does not resolve and does not shadow.** "Does not resolve" means it is not a candidate, not that the read returns 404: the walk continues past it, so a `GET .../secret` returns an ancestor's `shared` value when the chain offers one and the canonical 404 only when nothing in the chain does. That is the whole point — critically, the record does **not** hide an inherited value from an ancestor. A half-finished create in a child tenant must not silently break inheritance that was working before it started. This is the one rule with a claim on a *second* surface: the collection read reduces a reference's rows to the one a value read would resolve, so its reduction has to skip a `declared` local row in favour of a resolvable inherited one, or the listing would report the empty local record while the point read serves the ancestor's value ([ADR-0005](0005-cpt-cf-credstore-adr-upward-collection-read.md) "Reducing a reference to one item").
- **It is a distinct state, not "unfenced", and it is stored.** Today a `NULL` value fingerprint means "seeded out of band, serve on trust". A declared-but-empty record has no value at all, so the two cases must be told apart explicitly — by a lifecycle state on the record, not by the nullability of the fingerprint. Concretely: a fourth `status`, `declared`, widening the `CHECK (status IN (1, 2, 3))` the initial schema carries. Naming the state without giving it a column would leave every predicate that has to distinguish it — resolution, the reaper sweep, the collection read — resting on application logic over a value the metadata row does not hold; DESIGN §6.1 sets those predicates out. It is deliberately *not* a new nullable marker: `status` is already the lifecycle column, and the sweep and resolution filters that must exclude the state are already written against it, so they need no change at all.
- **The reaper must not sweep it.** With `declared` as its own status this falls out rather than being enforced: the sweep selects `provisioning` rows past their timeout, and a `declared` record is not one. Had the state been folded into `provisioning`, the sweep would have needed a timeout exception for a record whose value write may legitimately arrive days later — or never — and a slow operator would lose their record between the two calls.
- **Atomicity moves to the client.** The gear no longer guarantees "either both parts exist or neither"; a client that abandons the sequence leaves an empty record behind. The value write remains crash-safe on its own (backend write plus fingerprint stamp in one saga), so what is lost is only the coupling between the two requests.
- **The UI consequence is real**: an administrator who creates a credential in a console sees a two-step flow, and the console must either drive both calls or show the record as incomplete.
- **The state is reachable again, on purpose.** `DELETE /credentials/{ref}/secret` clears the value and returns an `active` record to `declared`, keeping its metadata, its category, its sharing and its reserved reference. It is the "reset" the integration administrator persona asks for and the kill switch a value-blind role needs: revoke a leaked credential now, re-inject a replacement later, without deleting and redeclaring the record. Its consequence is the one every `declared` record has — the reference stops resolving and stops shadowing, so a descendant that was receiving this tenant's override falls back to the ancestor's `shared` value, if any. Deleting the whole record would do the same and lose the metadata too.

If atomic creation is later required — for instance because empty records start appearing in production catalogues — it returns as a collection-level `POST` carrying both parts, and this ADR is amended rather than reinterpreted (see [Revisit Triggers](#revisit-triggers)).

### What a response says about tenants above: `owner_tenant_id` is dropped

Today's metadata carries `owner_tenant_id`, and for an inherited credential that is the identifier of an **ancestor** tenant. Both it and the `is_inherited` boolean beside it are dropped; `inheritance` (`own` / `inherited` / `overridden`) is the single field that answers the question either of them answered, and it answers it better — `is_inherited` cannot tell "mine, and nothing above" from "mine, and shadowing something above", which is precisely the distinction that decides what happens if the record is deleted.

The tenant identifier is a different matter, and it is dropped for a reason worth writing down rather than for tidiness.

**The caller has no other way to obtain it.** The gear reads the ancestor chain from the Tenant Resolver, in-process and with barriers ignored, which is a privilege it holds as a gear and the caller does not. The Tenant Resolver publishes no HTTP surface at all — no `RestApiCapability`, no routes, no OpenAPI document — so nothing outside the process can ask it for a chain, and it performs no authorization of its own, trusting the calling gear to have decided. Account Management does expose a tenant's `parent_id`, but one record at a time and under its own PDP: a caller learns its immediate parent only if it may read its own tenant record, and reaching a grandparent needs a grant on the parent's record first. So a chain is walkable only by someone already entitled to each step.

Putting the owning tenant in a credential response bypasses that, and it does so for any ancestor at any depth. The identifier is also the by-product of a privilege the caller does not hold: the gear fetches the chain with barriers ignored (`cpt-cf-credstore-adr-upward-collection-read`), and it may do so because a `shared` value is published downward regardless of barriers. A barrier-respecting traversal stops at a `self_managed` tenant, so a tenant at or below a barrier is never shown the tenants above that barrier; the gear sees them only because it looks past the barrier on the caller's behalf. Nothing here changes what a barrier means — it still isolates a customer's management from its parent, and data published as `shared` still flows down through it, exactly as ADR-0005 states — it only says that what the gear learned by looking past the barrier is not the caller's to keep. Handing it out would be the gear using its trusted position to disclose what the position was granted for, which is the confused-deputy shape.

**It can be reconstructed, by whoever is entitled to.** Nothing is permanently lost. A caller that holds the grants can walk Account Management upward, one authorized read per level, and match a reference against each tenant's catalogue to find where it lives. An operator investigating "where did this credential come from" works with operator rights and can do exactly that. What the removal takes away is the shortcut that skipped the authorization at every level; what it costs is several requests instead of one field, paid by the party that has the rights to make them.

**`version` and `updated_at` go the same way for an inherited record.** Both describe the ancestor's write activity — how many times it has rotated or edited the credential, and when it last did — which is operational detail about a tenant the caller cannot otherwise observe, and neither serves the caller: the CAS validator of an inherited record is useless to it, since it cannot write that record, and D4's guarantee is about the caller's *own* row. A weaker class of disclosure than the identifier, but the same shape, and dropping one while keeping the other would be inconsistent. So an inherited `Credential` carries neither field, and instead of the strong `"<id>.<version>"` its `ETag` is a **weak, opaque** validator: `W/"<hmac(id.version)>"` under a key kept for this purpose and nothing else. Two properties follow by construction. It still changes exactly when the ancestor writes, so a consumer can still ask "has this changed since I last looked" cheaply under `read`. And RFC 9110 requires the strong comparison for `If-Match`, so a weak validator can never be used to write — a client that tries gets the same refusal it would get for writing an ancestor's record anyway. The caller's own record keeps the strong validator and both fields. The collection read follows: `updated_at` is present on `own` and `overridden` items and absent on `inherited` ones, which is why ADR-0005 offers no ordering or filtering on it.

**Open question for the platform, not for this gear.** This reasoning holds only while the ancestor chain stays unpublished. If a Tenant Resolver HTTP API is added later and exposes `get_ancestors` to callers, the chain becomes obtainable directly and the argument above weakens or disappears — at which point re-adding `owner_tenant_id` would cost nothing and would be a convenience worth having. **This needs confirming with the Tenant Resolver's owners before this ADR is accepted:** is the upward chain intended to stay unavailable to a caller with minimal rights once that gear grows an API, or is it planned to be public? The answer changes nothing about the split itself, only about this one field.

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
- **The selector cannot widen scope.** Each match resolves through the ancestor chain exactly as a point read does, and every item is authorized as `read_secret` on its own resolved type and category. A filter therefore returns a subset of what N point reads would return; it saves round-trips, never privileges. For an application whose grant *is* "values of category `email-sender`", "all my SMTP secrets" discloses nothing it could not already fetch by name.
- **The selector vocabulary is deliberately small** and every field in it must be backed by an index: `references` in the body, or `$filter` over `category`, `type` and `reference` with `eq` / `in`. No `owner_tenant_id`, no ordering, no `$select`. Today `credstore_secrets` indexes none of `category`, `secret_type_uuid` or `sharing` individually, so shipping the filtered selector requires the migration that adds them; without it the filter is a sequential scan, and the platform checks the "indexed fields only" rule by review, not automatically.
- **No pagination and no cursor.** Values must not be walkable; a paginated value read is a catalogue dump in slow motion.
- **Hard cap with an explicit failure, never truncation.** Above the cap (proposed: 25) the request fails with `400 TOO_MANY_MATCHES` and the client is expected to narrow the selector. Truncation would both hide credentials from a legitimate caller and turn the endpoint into a drip.
- **The response is not a `Page<T>`.** No `items`/`page_info` envelope, no `next_cursor` — the type is a flat per-reference result list, so nothing in the contract suggests the result can be continued.
- **Per-item outcome under HTTP 200.** Not-resolved, out-of-scope, suppressed and fence-mismatch all report the same "not found" outcome per item (D3), and a failing item never aborts the batch.
- **The two selectors differ in what a refused item may say.** With the **explicit** selector the response carries one item per reference the caller supplied, "not found" included: echoing a name back to whoever just sent it discloses nothing. With the **filtered** selector a refused row must be **omitted entirely** rather than reported as a "not found" item, because its `reference` was never in the caller's hands — the filter found it, and naming it would turn a value endpoint into an oracle for names the caller may not read. The consequence is that the filtered form's item count is not a fixed function of the request, which is the same property the collection read already has (ADR-0005 "Pagination over a reduced result") and the reason neither surface reports a total.
- **Per-item fence.** Every returned value is verified against its row fingerprint (ADR-0003).
- **`Cache-Control: no-store`**, one audit record per value actually returned, and its own gateway rate-limit rule on its own path.
- **The cap is enforced by fetching `cap + 1`, never by counting.** No `COUNT` query is issued: if a `cap + 1`-th row comes back, the request fails; otherwise the rows fetched are the answer. This matches the platform rule against counting queries and keeps the check one indexed read.

One response shape serves both selectors — a per-item outcome, and beside each value the same usage envelope the point read carries (`type`, `expires_at`), nothing administrative:

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
      // The usage envelope only. `sharing`, `category`, `inheritance` and
      // `status` are the record's business and stay behind `read`; a value
      // reader learns what it needs to use the value and nothing more.
      "type": "gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~",
      "expires_at": null
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
- The value carries only its usage envelope (`type`, `expires_at`), so `read_secret` discloses a different representation from `read` rather than a superset of it; the two privileges stay distinct in fact, not only in name.
- Cost: this renames the shipped collection and moves the value read to a sub-resource. Every current consumer of the value changes. That is accepted under D1 and recorded below.
- Cost: one more endpoint than a flag-based design, and a `POST` used for a read. The latter has precedent in this codebase (`POST /usage-collector/v1/records/aggregate`) and has a side benefit for secrets: a `POST` response is not cached by intermediaries.
- Cost: the bulk endpoint performs up to the cap backend reads and fence verifications per request, plus one PDP evaluation per distinct type; it needs its own latency budget.
- Cost: bulk clients handle partial results, which is more code than N point reads that either succeed or 404.
- Cost: creating a credential is two requests with no atomicity between them, and "record without a value" becomes a legal state the domain, the reaper and the resolution rules all have to account for.
- Benefit of the same trade: metadata can be edited without touching the value at all, which is what makes a value-blind administrator role possible in the first place; with only a value-bearing `PUT` that role could not change a sharing mode without rewriting the secret.

### Confirmation

- Contract tests: no response schema under `/credentials` or `/credentials/{ref}` contains a `value` property; `value_fp` and `fp_key_id` appear in no schema at all.
- E2E: a role with `read` but not `read_secret` reads the record, obtains the `ETag`, completes a guarded `PUT …/secret`, and still receives 404 on `GET …/secret` — byte-identical to the 404 for a name that does not exist.
- E2E: the bulk endpoint rejects a selector matching more than the cap with `TOO_MANY_MATCHES`; an item the caller may not read is reported identically to an item that does not exist; a fence-poisoned item does not affect its siblings.
- E2E: an application granted `read_secret` on one category receives exactly its own credentials for a `$filter` on that category, and an empty result for another category.
- E2E: the bulk endpoint accepts `$filter` but rejects `cursor`, `$orderby`, `$top` and `limit`; its response carries no `page_info`.
- E2E: `PUT /credentials/{ref}` with `If-None-Match: *` returns 201 the first time and 409 the second; a `PUT` naming a different type is rejected; a `PUT` that omits expiry clears it; a `PUT` whose body equals the current record returns 204 with the same `ETag` and does not bump `version`.
- E2E: `PUT /credentials/{ref}` with `If-None-Match: *` returns 201 for a reference that currently resolves to an ancestor's `shared` credential — the override flow — and the record's `inheritance` reads `overridden` once its value is set.
- E2E: `PUT …/secret` with `If-None-Match: *` returns 204 on a `declared` record and 409 once a value exists; `PUT …/secret` with identical bytes still bumps `version`; `DELETE …/secret` returns the record to `declared`, keeps its metadata, and an inherited value resolves for descendants again.
- E2E: an inherited `Credential` carries neither `version` nor `updated_at`, its `ETag` is weak, that `ETag` changes when the ancestor rotates, and sending it in `If-Match` is refused; the caller's own record carries both fields and a strong `ETag`.
- Contract tests: the `Secret` schema contains exactly `reference`, `type`, `expires_at` and the value; `sharing`, `category`, `inheritance` and `status` appear in `Credential` only.
- E2E: a record created without a value returns 404 on its secret **and** an ancestor's inherited value keeps resolving for descendants while the record stays empty.
- E2E: the reaper leaves a deliberately empty record untouched across at least two sweep intervals.
- Audit: one record per value returned by both the point read and the bulk read; no record claims a disclosure when the response carried none.

## Backward Compatibility by Mode

Compatibility is recorded here, not optimized for (D1).

| Surface / mode | Compatible with what ships today | Detail |
|---|---|---|
| Value read | **no — moves and slims** | `GET /credstore/v1/secrets/{ref}` becomes `GET /credstore/v1/credentials/{ref}/secret`. Every consumer of a value changes its URL. The body becomes the `Secret` envelope — value, `reference`, `type`, `expires_at` — so `sharing`, `is_inherited` and `owner_tenant_id` no longer accompany a value; same `ETag`, same `no-store`. |
| Entity URL semantics | **no — inverts** | The item URL used to return the secret; it now returns the record. A client that keeps calling the old shape gets metadata, never a value, so the failure is a missing field rather than a silent disclosure. |
| Collection name | **no — renamed** | `secrets` → `credentials`. |
| `is_inherited` in the metadata | **no — replaced** | Superseded by `inheritance`, which distinguishes `own` from `overridden` where the boolean could not. A client reading the boolean reads one field's worth of a three-state answer. |
| `version`, `updated_at` on an inherited record | **no — removed** | Both described an ancestor's write activity. An inherited `Credential` carries a weak opaque `ETag` for change detection instead; own records keep both fields and the strong validator. |
| Clear value (`DELETE …/secret`) | n/a — new | Returns a record to `declared` without deleting it. |
| `owner_tenant_id` in the metadata | **no — removed** | For an inherited credential this named an ancestor tenant the caller cannot learn by any other route, including ancestors above a barrier that a barrier-respecting traversal would never show it. Recoverable by an entitled caller through Account Management, one authorized read per level. Pending confirmation with the Tenant Resolver's owners that the upward chain stays unpublished. |
| Create | **no — removed** | `POST /credstore/v1/secrets` with a value in the body disappears. Creation becomes `PUT /credentials/{ref}` + `If-None-Match: *` for the record, then `PUT …/secret` for the value: **two requests, no atomicity**. |
| Rotation | **no — moves** | `PUT /credstore/v1/secrets/{ref}` becomes `PUT /credstore/v1/credentials/{ref}/secret`; the `If-Match` contract itself is unchanged. |
| Metadata edit | n/a — new | Previously impossible without rewriting the value; now `PUT /credentials/{ref}`. |
| List, bulk read, suppression | n/a — new | Nothing to break. Listing is a documented non-goal today, so shipping it amends the PRD rather than breaking a contract. |
| SDK trait `CredStoreClientV1` | **no — reshaped** | The trait follows the two resources. `get`, `put`, `list`, `delete` address the record: `get` returns a `Credential`, which has no `value` field, so every existing caller of `get` fails to compile rather than silently reading metadata. `get_secret`, `put_secret` and `read_secrets` address the value. `create` is removed: a single-request create of record plus value is exactly what the split takes away. In-repo consumers (OAGW, the keycloak-idp plugin) move to `get_secret`. |
| **Secret type ids** | **no — renamed** | `gts.cf.core.credstore.secret.v1~` becomes `gts.cf.core.credstore.credential.v1~`, and every derived id, `SECRET_RESOURCE_TYPE` and `GENERIC_TYPE_UUID_STR` follow. Custom types re-register under the new base; stored `secret_type_uuid` values change (a constant change while there are no production rows, a migration afterwards). |
| **Authorization model (resource type and actions)** | **no — replaced** | The resource type changes, so no shipped permission matches any new operation. The six actions are `list`, `read`, `write`, `delete` on the record and `read_secret`, `write_secret` on the value. Every policy is re-issued against `credential.v1~`. |

**Why the authorization break is structural, not softened.** The tempting shortcut would be to accept the shipped `read` on the new surfaces so deployments roll forward untouched. It would leave a grant whose meaning is permanently ambiguous — a reviewer could not tell whether `read` was meant to include value disclosure — and since the premise of this ADR is that value disclosure is a separate privilege, an ambiguous grant defeats it. The type rename closes the question without a rule: shipped permissions name `secret.v1~`, the new operations authorize against `credential.v1~`, and the two never match. Re-granting is a release task with an explicit checklist: enumerate every role holding the shipped `read` or `write`, decide per role whether it needs the record, the catalogue, the value, or several, and issue the new grants.

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
- **AuthZ model:** six actions on the renamed resource type `gts.cf.core.credstore.credential.v1~…`: `list`, `read`, `write`, `delete` on the record; `read_secret`, `write_secret` on the value. Plain verbs for the entity follow the platform convention (`read`/`write`/`delete` in every other gear); the `_secret` suffix follows its compound-action pattern (`set_reaction`, `upload_attachment`) and names the sub-resource, never the entity. The bulk endpoint introduces **no new action** — it evaluates `read_secret` per item, so it can never grant more than N point reads.
- **Compatibility of the model:** deliberately not preserved; see the table above. The break is structural: a shipped permission on `secret.v1~` matches no operation on `credential.v1~`.
- **Escalation path to watch.** Whoever may change a credential's `category` changes which application may read its value, and under the scoped selector that also changes which credentials appear in an application's bulk result. That right belongs to `write`, its values come from a registry, and the PDP policy owner must treat it as privileged rather than cosmetic.

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

- **Schema changes are two, and both are named elsewhere in this ADR.** The fourth `status` value `declared` (the value-less record) and the type-id rename, which changes every stored `secret_type_uuid` ("Why `credentials` and not `secrets`"). No new column originates here; `category` comes from the companion work.
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
- The Tenant Resolver grows an HTTP API that exposes the ancestor chain to callers. The `owner_tenant_id` removal rests on that chain being unobtainable outside the process; once it is obtainable, the field costs nothing and can come back.
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
- Good: with the base type renamed alongside, the type, the collection, the schemas, the actions and the SDK share one noun for the entity and one for the payload; no layer has to translate.
- Bad: renames the collection and moves the value read; every HTTP consumer of a value changes.

### A3: `secrets` collection, value at `value`

- Good: the same structural benefit as A2 with a smaller rename — the collection name survives.
- Bad: muddled vocabulary, in which "secret" names the record and "value" names the secret; every reader has to hold that inversion in mind.
- Bad: keeps a name that will read oddly next to `credentials-storage` if the two converge.
- Bad: the entity's noun is the payload's noun, so `GET /secrets/{ref}` promises a secret and returns none of it; no schema name fully removes that reading.

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
- Bad: collapses `list` and `read_secret` into a single effective privilege (D2).
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
