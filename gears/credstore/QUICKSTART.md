# CredStore - Quickstart

Stores, retrieves, and deletes secrets scoped to tenants and owners. Secrets are resolved hierarchically — if a secret is not found in the requesting tenant, the gears walks up the tenant ancestry and returns the nearest inherited value.

**Features:**
- Tenant-scoped secret storage with hierarchical resolution
- Three sharing modes: `private` (owner only), `tenant` (all users in tenant), `shared` (cross-tenant)
- Access denial returned as `404` (not an error) to prevent secret enumeration
- Backend-agnostic: storage is delegated to a plugin selected by `vendor` configuration

**Use cases:**
- Storing API keys or credentials per tenant (e.g. `partner-openai-key`)
- Inheriting organization-wide secrets in child tenants without duplication
- Sharing secrets across tenant boundaries via `shared` mode

Full API documentation: <http://127.0.0.1:8087/cf/docs>

The example server uses the gear prefix `/cf`. This comes from `gears.api-gateway.config.prefix_path` and is configurable.

## Configuration

```yaml
gears:
  credstore:
    vendor: "constructorfabric"  # Selects backend plugin by vendor name (default: "constructorfabric")
```

## Examples

The examples below are the **current** surface — everything here works today.

### Store a Secret

```bash
curl -s -X POST "http://127.0.0.1:8087/cf/credstore/v1/secrets" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"reference": "partner-openai-key", "value": "sk-abc123", "sharing": "tenant"}'
```

Response: **201 Created** (`Location: …/credstore/v1/secrets/partner-openai-key`)

### Update (Rotate) a Secret

Updates require an `If-Match` precondition: the current `ETag` from GET for a guarded compare-and-set, or `*` for an explicit last-writer-wins overwrite. A `PUT` never creates — use `POST` above.

```bash
curl -s -X PUT "http://127.0.0.1:8087/cf/credstore/v1/secrets/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H 'If-Match: *' \
  -d '{"value": "sk-def456", "sharing": "tenant"}'
```

Response: **204 No Content** (missing `If-Match` → **400** `IF_MATCH_REQUIRED`; stale `ETag` → **409** `OPTIMISTIC_LOCK_FAILURE`)

### Retrieve a Secret

```bash
curl -s "http://127.0.0.1:8087/cf/credstore/v1/secrets/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" | python3 -m json.tool
```

**Output:**
```json
{
    "value": "sk-abc123",
    "owner_tenant_id": "a1b2c3d4-0000-0000-0000-000000000000",
    "sharing": "tenant",
    "is_inherited": false
}
```

`is_inherited: true` indicates the secret was resolved from an ancestor tenant.

### Delete a Secret

```bash
curl -s -X DELETE "http://127.0.0.1:8087/cf/credstore/v1/secrets/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'If-Match: *'
```

Response: **204 No Content** (`If-Match` is mandatory here too: an `ETag` for a guarded delete, `*` to delete whatever is there)

For additional endpoints, see <http://127.0.0.1:8087/cf/docs>.

## Planned surface (ADR-0004 / ADR-0005)

The examples in this section describe a **proposed** direction — [ADR-0004](docs/ADR/0004-cpt-cf-credstore-adr-secret-value-exposure.md)
and [ADR-0005](docs/ADR/0005-cpt-cf-credstore-adr-upward-collection-read.md) —
and are **not implemented**. Nothing below will work against the server today;
the current surface is `/credstore/v1/secrets…`, shown above. Under the
proposed model a credential record and its secret value become separate
resources, addressed under `/credstore/v1/credentials…`.

> **Creating a credential is one request:** `PUT .../credentials/{ref}` with
> `If-None-Match: *` carries the record and its value together, written
> atomically. A merge-`PATCH` on the same address —
> `Content-Type: application/merge-patch+json` — edits metadata, rotates the
> value, or removes it (`{"value": null}`) without recreating the record; it
> never creates, so a `PATCH` against a reference with no own record is a
> 404. Both verbs are checked against the same `ETag` — there is one
> resource and one validator, not a record and a separate value
> sub-resource.

**List credential records** — demonstrates the new metadata listing, bounded
with `limit` and filtered with the platform `$filter` syntax; the response
never carries a value. Requires the `list` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0005
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials?limit=20&\$filter=type+eq+'gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~'" \
  -H "Authorization: Bearer $TOKEN"
```

**Get one credential record** — demonstrates the point metadata read; the
response carries no value but does carry the `ETag`, which a value-blind
caller uses as the CAS validator for a later write. Requires the `read`
PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -si "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN"
# → 200, body has no "value" field; response carries an ETag header
```

**Read the secret value** — demonstrates the value read moved to its own
sub-resource address, distinct from the record; the body carries the value
with its type and expiry only, and the record's `ETag` in the header.
Requires the `read_secret` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key/secret" \
  -H "Authorization: Bearer $TOKEN"
```

**Create a credential** — demonstrates atomic creation: the record and its
value are written together, under the create-only precondition. Requires
both the `write` and `write_secret` PDP actions.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PUT "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H 'If-None-Match: *' \
  -i \
  -d '{"sharing": "tenant", "type": "gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~", "value": "sk-abc123"}'

# 201 Created
# Location: /cf/credstore/v1/credentials/partner-openai-key
# ETag: "7f3a…-…-…c1.1"     <- the validator later writes need
```

**Rotate the value** — demonstrates a guarded partial update carrying only
`value`, keyed off the record's `ETag` (from the record read above, or from
the `201` of creation above). `PATCH` follows RFC 7396 merge-patch
semantics — fields absent from the body are untouched — so this call
changes nothing but the value; it always writes and bumps `version`, even on
identical bytes. Requires the `write_secret` PDP action, and notably not
`read_secret` — this is the write a value-blind configurator performs.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PATCH "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/merge-patch+json" \
  -H 'If-Match: "<etag-of-the-record>"' \
  -d '{"value": "sk-def456"}'
```

**Edit metadata without touching the value** — demonstrates a partial update
that carries no `value` key at all: the value is left exactly as it was, so
this call can be made by a caller holding `write` but not `write_secret`.
Requires the `write` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PATCH "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/merge-patch+json" \
  -H 'If-Match: "<etag-of-the-record>"' \
  -d '{"sharing": "tenant"}'
```

**Remove the value, keep the record** — demonstrates the only way to reach
the value-less `declared` state: a partial update whose `value` is `null`.
The record's metadata and `fallback` are untouched; what the
reference then resolves to is decided by `fallback`, not by this call.
Requires the `write_secret` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PATCH "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/merge-patch+json" \
  -H 'If-Match: "<etag-of-the-record>"' \
  -d '{"value": null}'
```

**Bulk read secret values, explicit selector** — demonstrates the bounded,
non-paginated bulk value read: selecting `secret` on the collection switches
it into value mode, scoped here by an explicit `reference in (...)` list.
Requires `read_secret`, evaluated per item; `limit`/`cursor` are rejected;
cap 25.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials?\$filter=reference+in+('smtp-default','stripe-key','webhook-signing')&\$select=reference,type,expires_at,secret" \
  -H "Authorization: Bearer $TOKEN"
```

**Bulk read secret values, scoped selector** — demonstrates the same value
mode scoped by `type` instead of an explicit reference list; still capped,
still per-item authorized. Requires `read_secret`, evaluated per item;
`limit`/`cursor` are rejected; cap 25.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials?\$filter=type+eq+'<gts id>'&\$select=reference,type,expires_at,secret" \
  -H "Authorization: Bearer $TOKEN"
```
