# RBAC

The RBAC gear is the platform's source of truth for role-based access control on the
management plane. It owns role definitions and role assignments, and answers the
permission questions a policy decision point asks: which roles a subject holds in a
tenant context, and whether a given `(operation, resource_type)` is permitted at a scope.

It does not authenticate anyone, does not decide requests on its own, and stores no
resource inventory. A PDP — the AuthZ Resolver Plugin, or your own — consumes its
in-process client and turns the answers into decisions.

## Model

- **Role definition** — a named set of `permissions` (`operation` + `target_type`) and
  optional `not_permissions` (explicit denies), plus the `assignable_scopes` it may be
  granted at. Built-in roles are platform-owned and immutable; custom roles are owned by
  a tenant.
- **Role assignment** — a `(principal, role, scope)` triple. Principals are `User`,
  `Group`, or `ServicePrincipal`; the id is an opaque string, matched verbatim against
  what the PDP presents.
- **Scope** — `/` (global), `/tenants/{uuid}`, or
  `/tenants/{uuid}/resourceGroups/{uuid}`. Scopes inherit downward with no
  per-assignment opt-out.
- **`target_type`** — a GTS identifier: a concrete type (`gts.….v1~`) or a family
  wildcard (`gts.….*`). Matching is prefix-based with the separator retained, so
  `…compute.*` never matches `…computer…`. There is no bare `*`.

## Requirements

The gear declares `deps = [types_registry, tenant_resolver, resource_group]` and resolves
each from `ClientHub` at `init()`; a missing one is a startup error, not a degraded mode.

- **types-registry** — resolves `target_type` when a custom role is written, and serves
  the permission catalogue (`gts.cf.toolkit.authz.permission.v1~` instances).
- **tenant-resolver** — validates tenant scopes and walks the tenant hierarchy.
- **resource-group** — resolves resource-group scopes and group membership.

Postgres is the production backend. SQLite works for tests, development, and embedded
demos, but the migrations drop the GIN / `pg_trgm` / `text_pattern_ops` indexes there, so
`LIKE` filtering degrades to full scans.

## Configuration

```yaml
gears:
  rbac:
    database:
      server: "pg_main"
    config: {}          # every field below is optional; `{}` is a valid whole config
```

Note the `{}`. A `config:` key with nothing under it is YAML `null` and fails
deserialization with a message that names no field — write `config: {}` or omit the key.

### Who administers the platform

```yaml
    config:
      platform_admin_subject_id: "9f1c0f7a-2c5f-4a0e-9c9f-1d6f2b7e4a10"
```

When set, `init()` idempotently grants that subject the built-in `Owner` role at scope
`/`. There is deliberately no default — a phantom one would hand someone the platform.

Prefer injecting the value over committing a literal. The `APP__` environment layer
overrides any config key, so
`APP__GEARS__RBAC__CONFIG__PLATFORM_ADMIN_SUBJECT_ID=<subject-id>` supplies it at
startup. Note that `${VAR}` placeholders are **not** expanded inside a gear's `config`
block — expansion covers database DSNs and passwords only — so `"${SOMETHING}"` written
here would be stored verbatim as the subject id.

If your identity provider also needs the same person bound on its side (a realm admin,
for example), that is a separate setting in whichever gear owns the IdP integration.
Nothing validates that the two name the same subject; the host must pass one value to
both.

### Grants written at startup

```yaml
    config:
      seed_integration_roles: true
      service_principal_grants:            # principal_type = ServicePrincipal
        - role: "Credstore Secret Operator"
          principal_id: "1d70b6d4-6e2e-4f3c-9aa3-7d8c2e3f5b91"
      user_grants:                         # principal_type = User
        - role: "Reader"
          principal_id: "9f1c0f7a-2c5f-4a0e-9c9f-1d6f2b7e4a10"
```

In-process actors (a plugin writing secrets, a collector emitting usage records) need
real grants so their writes are authorized through ordinary policy instead of a PEP
bypass. Human operators need one so a fresh deployment has somebody able to sign in and
administer it before any API call is possible. Which principals exist, and under which
subject ids, belongs to the deployment, so both lists are empty by default and every
entry is validated at startup: a blank field, an unknown role name, or a role this
deployment does not seed aborts `init()`, naming the list and the index.

The two lists differ only in the `principal_type` they write, and that is exactly why
they are separate. The type has to match what the caller's token classifies as — `user`
in the IdP claim becomes `User`, `service` / `service_principal` becomes
`ServicePrincipal`. A grant filed under the wrong one is never found: the request is
denied, and nothing anywhere says why. A single list with a `principal_type` field would
make that a typo away.

Both write at scope `/` only. A tenant-scoped grant would name a tenant that need not
exist when RBAC starts, and this path writes straight to the table without the
scope-existence check the REST handler performs — so those belong on
`POST /rbac/v1/role-assignments`.

Config wins over the API here: a grant removed with `DELETE /rbac/v1/role-assignments/{id}`
comes back on the next restart. To revoke one for good, take it out of the config.

### What the built-in roles grant

| Role | Grants | Assignable at |
|------|--------|---------------|
| `Owner` | `*` on `builtin_role_targets.platform` | `/` |
| `Contributor` | `*` on `builtin_role_targets.resources_family` | `/` |
| `Reader` | `read` on `builtin_role_targets.resources_family` | `/` |
| `User Access Administrator` | `*` on role assignments, `read` on role definitions | `/` |
| `Credstore Secret Operator`¹ | `read`/`write`/`delete` on the credstore secret type | `/` |
| `Usage Emitter`¹ | `create` usage records, `read` usage types | `/` |

¹ Seeded only when `seed_integration_roles: true`. Their targets are another gear's
resource types, so a platform without those gears would otherwise inherit roles that
authorize types nobody registered.

Two of those targets are named by the platform, not by RBAC:

```yaml
    config:
      builtin_role_targets:
        platform:                          # what Owner grants
          - "gts.cf.*"
          - "gts.vendor.*"
        resources_family:                  # what Contributor / Reader grant
          - "gts.vendor.resources.*"
```

A deployment that registers its types under its own vendor **must** set these, or the
three roles authorize nothing: matching is by prefix, so `gts.cf.*` never covers
`gts.vendor.…`. Each entry is a family wildcard or a concrete type id, checked at
`init()`; an empty list and the bare `gts.*` are both refused. A rule over one of these
slots expands into one permission rule per entry.

**Keep `gts.cf.*` in `platform` unless you mean to drop it.** RBAC's own
`role_definition` and `role_assignment` types are `gts.cf.core.…` whatever vendor the
deployment publishes under, and `Owner` is a wildcard over this list — so a `platform`
naming only your own family produces an `Owner` who cannot create role assignments,
including the ones that would fix it. `init()` warns when no entry covers RBAC's own
types; the alternative is to administer RBAC through `User Access Administrator`, whose
targets are fixed and unaffected by this setting.

The default `resources_family` deserves a warning too: **nothing in this repository
registers a type under `gts.cf.resources.*`**, so with the defaults `Contributor` and
`Reader` are empty roles. Point the setting at your own resource plane to give them
meaning.

Role ids and names are *not* configurable. They are a cross-deployment contract — the
platform-admin bootstrap resolves `Owner` by id, and a partial unique index enforces
built-in name uniqueness — so renaming one is a breaking change for every consumer.

### Display names on reads

A role-assignment read carries `principal_name`, `created_by_name` and
`role_definition_name` alongside the ids, and a role-definition read carries
`assignment_count`. Everything under `principal_names` is optional:

```yaml
config:
  principal_names:
    enabled: true                        # false serves ids and resolves no upstream client
    cache_ttl_seconds: 30                # a rename shows up within one TTL
    cache_max_entries: 10000
    max_pages_per_tenant: 5              # 200 users per page -> first 1000 members per pass
    max_point_lookups_per_tenant: 25     # fallbacks after a truncated pass
    max_lookup_tenants_per_request: 8    # distinct tenants named in one request
    resolve_timeout_ms: 5000             # wall clock for one request's naming
```

Names are decoration: a field is simply absent when it cannot be resolved — an exhausted
budget, an upstream outage, an id nothing can name — and never changes the status code, the
rows, or the cursor. `ServicePrincipal` principals are never named: the platform has no
`subject_id` to `client_id` reverse lookup.

The defaults exist because naming a user is not a point read. Account management serves a
user listing out of the tenant's group membership and re-drains it per call, so names are
resolved in one batched pass per tenant rather than one lookup per row. Zero is refused for
every bound at `init()` — a zero page budget runs no pass at all and turns every read into
per-id lookups, which is the N+1 the pass exists to prevent.

Account management is *not* in the gear's `deps` (that edge would close a dependency
cycle); its client is resolved from `ClientHub` at first use, and a deployment without it
simply serves ids.

## Audit trail

Rows written at startup are distinguishable from anything a human created:

- `created_by = "system"` — role definitions written by the built-in seeder;
- `created_by = "system-bootstrap"` — the platform-admin and service-principal
  assignments.

## Consuming it

Depend on `rbac-sdk` and resolve `RbacServiceClientV1` from `ClientHub`. The SDK is
transport-free: it carries the models, the error enum, and the client trait, and nothing
else. The REST surface under `/rbac/v1` is the administrative API — role CRUD, assignment
CRUD, the catalog counts at `GET /rbac/v1/role-definitions/summary`, and the permission
catalogue — not the path a PDP should take.

Errors follow the platform's canonical taxonomy
(`gts.cf.core.errors.err.v1~cf.core.err.<category>.v1~`) as RFC 9457 problem documents;
`context.resource_type` names the RBAC resource so clients can branch without parsing
prose.

## Further reading

- [Design](../docs/DESIGN.md) — API contracts, data model, algorithms, and the status
  mapping the handlers actually return
- [Requirements](../docs/PRD.md) — scenarios and acceptance criteria
- [Entity schemas](../docs/schemas/) — the GTS JSON Schemas registered at startup

## License

Apache-2.0
