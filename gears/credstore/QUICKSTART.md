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

> **Creating a credential becomes two requests, with no atomicity between
> them:** first the record (`PUT .../credentials/{ref}`), then its value
> (`PUT .../credentials/{ref}/secret`). A client that stops after the first
> call leaves a value-less record behind; that state is legal and does not
> shadow an inherited value.
>
> The second request needs a precondition, and the first supplies it: record
> creation returns the `ETag`, and a value write's precondition is evaluated
> against the *record's* validator, because a record and its value share one
> version. So the flow is genuinely two requests — no metadata `GET` in
> between to fetch a validator.

**List credential records** — demonstrates the new metadata listing, bounded
with `limit` and filtered with the platform `$filter` syntax; the response
never carries a value. Requires the `list` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0005
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials?limit=20&\$filter=category+eq+'email-sender'" \
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
sub-resource address, distinct from the record. Requires the `read_secret`
PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key/secret" \
  -H "Authorization: Bearer $TOKEN"
```

**Rotate the value** — demonstrates a guarded value write, keyed off the
`ETag` of the **record** (from the record read above, or from the `201` of
record creation below). The value sub-resource has no validator of its own:
a record and its value share one version, so the record's `ETag` is what a
value write is checked against. Requires the `write_secret` PDP action, and
notably not `read_secret` — this is the write a value-blind configurator
performs.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PUT "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key/secret" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H 'If-Match: "<etag-of-the-record>"' \
  -d '{"value": "sk-def456"}'
```

**Create a record (no value yet)** — demonstrates create-only semantics on
the record resource, the first of the two calls a new credential needs.
Requires the `write` PDP action.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X PUT "http://127.0.0.1:8087/cf/credstore/v1/credentials/partner-openai-key" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -H 'If-None-Match: *' \
  -i \
  -d '{"sharing": "tenant", "type": "gts.cf.core.credstore.credential.v1~cf.core.credstore.basic_auth.v1~"}'

# 201 Created
# Location: /cf/credstore/v1/credentials/partner-openai-key
# ETag: "7f3a…-…-…c1.1"     <- the validator the value write below needs
```

**Bulk read secret values, explicit selector** — demonstrates the bounded,
non-paginated bulk read with a request body naming exact references.
Requires the `read_secret` PDP action, evaluated per item.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X POST "http://127.0.0.1:8087/cf/credstore/v1/credentials:read-secrets" \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"references": ["smtp-default", "stripe-key", "webhook-signing"]}'
```

**Bulk read secret values, filtered selector** — demonstrates the same bulk
read scoped by `$filter` on an indexed metadata field instead of an explicit
list; still capped, still per-item authorized. Requires the `read_secret` PDP
action, evaluated per item.

```text
# NOT IMPLEMENTED — planned, ADR-0004
curl -s -X POST "http://127.0.0.1:8087/cf/credstore/v1/credentials:read-secrets?\$filter=category+eq+'email-sender'" \
  -H "Authorization: Bearer $TOKEN"
```
