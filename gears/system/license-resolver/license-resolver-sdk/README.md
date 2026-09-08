# cf-gears-license-resolver-sdk

Public, transport-agnostic contract for the `license-resolver` gear — a
read-only, plugin-delegating resolver that answers a single question:
**is this resource licensed to this subject right now?**

## What this crate provides

- `LicenseResolverClient` — the public API trait, one method:
  `is_licensed(LicenseCheckRequest) -> LicenseDecision`.
- `LicenseResolverPluginClient` — the backend plugin trait (same signature).
- `LicenseCheckRequest` / `LicenseDecision` and the `Subject` / `Resource` /
  `LicenseCheckContext` contract objects.
- `LicenseResolverError` — the typed error enum with a default mapping to the
  canonical RFC-9457 error (`impl From<LicenseResolverError> for CanonicalError`;
  render a `Problem` via `Problem::from_error(&err.into())`). A not-granted answer
  is **not** an error (`LicenseDecision { granted: false }`).
- `LicenseResolverPluginSpecV1` — the GTS plugin spec used for discovery.
- The licensing base types `gts.cf.core.lic.subj.v1~` / `gts.cf.core.lic.res.v1~`
  (`LicenseSubjectV1<M>` / `LicenseResourceV1<M>`) that consuming Gears derive
  their concrete Subject / Resource contract types from.

## Identity model

Each `Subject` / `Resource` contract object carries:

| Field | Meaning |
|---|---|
| `type` (Rust: `gts_type`) | the derived `…subj.v1~…` / `…res.v1~…` contract type it instantiates; the resolver resolves it to validate `metadata` and read `admitted_subjects` |
| `id` | optional instance id (well-known name or UUID); absent ⇒ a whole-type check |
| `metadata` | open object; a derived contract refines the schema inside it |

The base types are abstract (`x-gts-abstract`) and generic over `metadata` (like
`PluginV1<P>`): a consuming Gear derives a contract type via
`#[gts_type_schema(base = LicenseResourceV1, …)]` and declares only its metadata
content fields. 

There is intentionally **no** listing/enumeration method: a platform's
licensing surface is enumerable from the types registry (every contract derives
from the base types), not from this API.

## Declaring your licensing contracts

A Gear that wants license enforcement registers one Subject and one Resource
contract type. Both derive from the base types and declare only their *metadata*
fields — the identity fields come from the base.

```rust
#[gts_type_schema(
    dir_path = "schemas",
    base = LicenseSubjectV1,
    type_id = gts_id!("cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"),
    description = "LLM gateway user subject contract",
    properties = "category",
)]
struct UserSubjectV1 { category: String }

#[gts_type_schema(
    dir_path = "schemas",
    base = LicenseResourceV1,
    type_id = gts_id!("cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~"),
    description = "LLM gateway model-usage resource contract",
    properties = "model_vendor,model_name",
    // Which Subject contracts may be checked against this Resource.
    traits = serde_json::json!({
        "admitted_subjects": ["gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"]
    }),
)]
struct ModelUsageResourceV1 { model_vendor: String, model_name: String }
```

The Resource contract's emitted schema (abridged):

```json
{
  "$id": "gts://gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~",
  "allOf": [
    { "$ref": "gts://gts.cf.core.lic.res.v1~" },
    { "properties": { "metadata": {
        "properties": { "model_vendor": {"type":"string"}, "model_name": {"type":"string"} },
        "required": ["model_vendor", "model_name"] } } }
  ],
  "x-gts-traits": {
    "admitted_subjects": ["gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~"]
  }
}
```

Note where things live: your fields land **inside** `metadata`, and
`admitted_subjects` sits beside the schema as a property of the *type* — it is
never part of a payload.

## `admitted_subjects`

A trait on the Resource contract type listing the Subject **contract** types
(i.e. legal `subject.type` values) it may be checked against. It is enforced
twice:

- **At registration** — each entry carries an `x-gts-ref` to
  `gts.cf.core.lic.subj.v1~`, so the types registry checks the *shape* of every
  id: pointing it at a *domain* type by mistake
  (`gts.cf.genai.llm_gateway.user.v1~`) fails there. Only the shape — a
  well-formed but unregistered id, or the abstract base itself, passes, and then
  denies at check time.
- **At check time** — the gateway resolves the Resource contract, and if
  `subject.type` is not in the list the request is rejected as `InvalidRequest`
  before any plugin is called.

That rejection is a **validation error, not a not-granted decision**. Without the
trait, a mismatched pair would reach a backend that knows nothing about it and
come back `granted: false` — a request-assembly bug disguised as a licensing
answer.

### A conforming check

```json
{ "subject":  { "type": "gts.cf.core.lic.subj.v1~cf.genai.llm_gateway.user.v1~",
                "id": "acme-admin", "metadata": { "category": "internal" } },
  "resource": { "type": "gts.cf.core.lic.res.v1~cf.genai.llm_gateway.model_usage.v1~",
                "id": "gpt-4o",
                "metadata": { "model_vendor": "openai", "model_name": "gpt-4o" } },
  "context":  { "tenant_id": "8f1b2c34-5d6e-4f70-8a91-b2c3d4e5f607" } }
```

Resolve the Resource contract by `resource.type` → validate `metadata` against
it → check `subject.type` against `admitted_subjects` → delegate.

Swap the subject for `gts.cf.core.lic.subj.v1~cf.core.am.tenant.v1~` and the same
request is rejected: that contract is not admitted.

### Omitting the trait denies everything

> **A derived Resource contract MUST declare `admitted_subjects`.** An empty list
> means *no subject is admitted*, not *not configured yet*.

A derived type that declares no `traits` emits no `x-gts-traits` at all, so trait
resolution falls back to the abstract base's declared value — which is `[]`,
required there because the GTS store rejects a trait schema with no values
anywhere in the derivation chain. The contract then registers cleanly, validates
`metadata` correctly, and rejects **every** check as "subject not admitted".

Widening the list later is non-breaking; narrowing it requires a new contract
version.

## Design

See `../docs/PRD.md`, `../docs/DESIGN.md`, and `../docs/ADR/`. This crate is the
SDK contract; the main gateway gear and reference backend plugin follow.
