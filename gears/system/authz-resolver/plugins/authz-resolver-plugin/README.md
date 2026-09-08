# AuthZ Resolver Plugin

A policy decision point (PDP) for the AuthZ Resolver gateway, backed by the RBAC gear.
It answers one question — may this subject perform this action on this resource? — and,
when the answer is yes, returns the constraints a policy enforcement point must apply to
scope the data it reads.

The gateway discovers it through the types-registry: the plugin registers a
`gts.cf.toolkit.plugins.plugin.v1~cf.core.authz_resolver.plugin.v1~cf.builtin.authz_resolver.plugin.v1`
instance at `init()` and publishes itself in `ClientHub`.

## What it does per request

1. **Validate** the request shape — subject type, action, resource type.
2. **Enforce token scopes** — the presented scopes must authorize the action.
3. **Validate the GTS resource type** against the types-registry (three modes:
   `strict` — the default — plus `warn` and `off`), with a cache in front. A registry
   outage fails closed in every mode that consults it.
4. **Evaluate permissions** through `RbacServiceClientV1` — roles, scopes, and
   `not_permissions` denies.
5. **Check the allow's provenance** — the aggregate scope must follow from the role
   assignments that produced it, checked before any hierarchy read so a malformed allow
   cannot widen into platform-root access.
6. **Generate constraints** — tenant subtrees and resource-group closures the PEP joins
   against, materialized within a configured expansion ceiling. An allow that resolves to
   no accessible identifier denies instead of becoming an unconstrained allow.

Denials carry a machine-readable code in `context.deny_reason.error_code`, all in one
namespace: `gts.cf.core.errors.err.v1~cf.authz.errors.<name>.v1`.

## Requirements

`deps = [types_registry, authz_resolver, rbac, tenant_resolver, resource_group]`. Four
clients are resolved from `ClientHub` at `init()` and a missing one is a startup error:
`RbacServiceClientV1`, `TenantResolverClient`, `ResourceGroupReadHierarchy`, and
`TypesRegistryClient`.

## Configuration

```yaml
gears:
  authz-resolver-plugin:
    config:
      vendor: "constructorfabric"   # REQUIRED — no default
      priority: 100
```

### `vendor` — how the gateway picks this plugin

`vendor` is **not** part of any GTS identifier. It is the payload field the gateway
matches on: `authz-resolver` reads its own `vendor` setting, collects every registered
plugin instance whose `vendor` field equals it, and takes the lowest `priority`.

So this value **must equal `gears.authz-resolver.config.vendor`** in the same deployment
(its default is `"constructorfabric"`). Mismatch them and the plugin registers
successfully but is never selected — the failure surfaces later, as
`no plugin instances found for vendor '…'` on the first authorization call. There is no
default here on purpose: an inherited one would make that mismatch quiet.

### Trusted system actors

```yaml
    config:
      trusted_system_actors:
        - subject_type: "am.system"
          subject_id: "00000000-0000-cf01-0000-616d73797374"
```

A request whose `(subject_type, subject_id)` matches an entry skips scope enforcement,
skips subject-type classification, and short-circuits to Allow. That is the widest bypass
in the plugin, so the list is **empty by default** — nothing is trusted unless a
deployment names it, and the count is logged at startup.

Both halves must match within one entry. The subject id is the load-bearing half: it is
minted in-process and never issued to a token holder, so a forged `subject_type` alone
cannot ride the bypass. Configure it only for actors the platform itself constructs —
typically a cascade or cleanup worker whose reads are PEP-gated but which holds no roles.

### Everything else

`cache` (hierarchy-read TTL, entry ceiling, singleflight, and the reserved event
invalidation switch), `gts_validation.mode`, `scope_enforcement`
(including the wildcard scope), `capability_degradation.max_expansion_ids`, and
`audit.enabled` all have defaults; see `src/config.rs`, where every field is documented
and unknown keys are rejected at startup.

## Consuming it

The plugin implements `authz_resolver_sdk::AuthZResolverPluginClient`. Enforcement points
use the SDK's PEP helpers rather than calling the plugin directly; the gateway routes to
whichever plugin its `vendor` selects.

## Further reading

- [Design](docs/DESIGN.md) — evaluation pipeline, constraint generation, caching, metrics
- [Requirements](docs/PRD.md) — functional requirements and acceptance criteria
- [RBAC gear](../../../rbac/rbac/README.md) — the permission data this plugin consumes

## License

Apache-2.0
