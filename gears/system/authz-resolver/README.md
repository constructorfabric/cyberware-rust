<!-- Updated: 2026-04-07 by Constructor Tech -->

# AuthZ Resolver

Authorization resolution for Gears — evaluates access requests and produces SQL-compilable constraints.

## Overview

The authorization design that Gears operate on is described in [DESIGN.md](../../../docs/arch/authorization/DESIGN.md).
The decision to use AuthZEN 1.0 with constraint extensions is documented in
[ADR-0001](../../../docs/arch/authorization/adr/0001-pdp-pep-authorization-model.md) and
the split from authentication in [ADR-0002](../../../docs/arch/authorization/adr/0002-split-authn-and-authz-resolvers.md).

The **authz_resolver** gear is an integration point for authorization policy evaluation. It discovers and delegates to a vendor-specific plugin via the types-registry — the plugin acts as the **Policy Decision Point (PDP)**, evaluating Subject + Action + Resource requests and returning a decision with optional row-level constraints.

## Public API

The gear registers [`AuthZResolverApi`](authz-resolver-sdk/src/api.rs) in ClientHub:

- `evaluate(ctx, request)` — Evaluate an authorization request (`ctx` carries the caller's `SecurityContext`) and return a decision with constraints

### Evaluation Model (AuthZEN-based)

**Request:**

```rust
EvaluationRequest {
    subject: Subject { id, subject_type, properties },
    action: Action { name },               // "list", "get", "create", "update", "delete"
    resource: Resource { resource_type, id, properties },
    context: EvaluationRequestContext {
        tenant_context,                    // Tenant hierarchy context
        token_scopes,                      // OAuth scopes from SecurityContext
        require_constraints,               // true → PDP must return constraints
        capabilities,                      // [TenantHierarchy, GroupMembership, ...]
        supported_properties,              // Properties the PEP can compile
        bearer_token,                      // Optional for PDP validation
    },
}
```

**Response:**

```rust
EvaluationResponse {
    decision: bool,
    context: EvaluationResponseContext {
        constraints: Vec<Constraint>,      // Row-level constraints (OR'd)
        deny_reason: Option<DenyReason>,
    },
}
```

### Constraints

Constraints are SQL-compilable row-level predicates returned by the PDP:

- **`Constraint`** — A set of predicates (AND'd together)
- **Multiple constraints** — OR'd to form the final scope
- **Predicates:** `Eq(property, value)` and `In(property, values)`

See [`constraints.rs`](authz-resolver-sdk/src/constraints.rs) for types.

### Errors

See [`error.rs`](authz-resolver-sdk/src/error.rs): `Denied`, `NoPluginAvailable`, `ServiceUnavailable`, `Internal`

## Policy Enforcement Point (PEP)

The SDK provides [`PolicyEnforcer`](authz-resolver-sdk/src/pep/enforcer.rs) — a high-level API that encapsulates the full PEP flow: build request, evaluate via PDP, compile constraints to `AccessScope`.

```rust
use authz_resolver_sdk::pep::{PolicyEnforcer, ResourceType};
use toolkit_security::pep_properties;

const USER: ResourceType = ResourceType::from_static(
    "gts.cf.core.users.user.v1~",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);

let enforcer = PolicyEnforcer::new(authz_client.clone());

// LIST — get scoped access
let scope = enforcer.access_scope(&ctx, &USER, "list", None).await?;
let users = secure_conn.find::<User>(&scope)?.all(conn).await?;

// CREATE — require constraints for tenant scoping
let scope = enforcer.access_scope(&ctx, &USER, "create", None).await?;
secure_conn.insert::<User>(&scope, user).await?;
```

### PEP Compiler

The [compiler](authz-resolver-sdk/src/pep/compiler.rs) converts PDP constraints into `AccessScope` for SecureORM with fail-closed guarantees:

| `decision` | `constraints` | `require_constraints` | Result |
|------------|---------------|----------------------|--------|
| `false` | any | any | `Err(Denied)` |
| `true` | empty | `false` | `Ok(allow_all)` |
| `true` | empty | `true` | `Err(ConstraintsRequired)` |
| `true` | present | any | Compile to `AccessScope` |

Unknown properties cause the containing constraint to fail. If all constraints fail, the request is denied (fail-closed).

## Plugin API

Plugins implement [`AuthZResolverPluginClient`](authz-resolver-sdk/src/plugin_api.rs) and register via GTS.

Gears include one plugin out of the box:
- [`static_authz_plugin`](plugins/static-authz-plugin/) — Tenant-scoped plugin for development and testing (denies access when no valid tenant is resolved)

## Configuration

### AuthZ Resolver Gear

See [`config.rs`](authz-resolver/src/config.rs)

```yaml
gears:
  authz_resolver:
    vendor: "constructorfabric"  # Selects plugin by matching vendor
```

### Static AuthZ Plugin

See [`config.rs`](plugins/static-authz-plugin/src/config.rs)

```yaml
gears:
  static_authz_plugin:
    vendor: "constructorfabric"
    priority: 100
```

## Usage

Most gears should use `PolicyEnforcer` (see PEP section above) rather than calling `evaluate` directly:

```rust
// Direct evaluation (low-level)
let authz = hub.get::<dyn AuthZResolverApi>()?;

let response = authz.evaluate(ctx.clone(), EvaluationRequest {
    subject: Subject { id: ctx.subject_id, ..Default::default() },
    action: Action { name: "list".into() },
    resource: Resource { resource_type: "my_gear.entity".into(), ..Default::default() },
    context: EvaluationRequestContext::default(),
}).await?;
```

## Implementation Phases

### Phase 1: Core (Current)

- AuthZEN-based `evaluate` API
- `PolicyEnforcer` with `access_scope` for one-step PEP flow
- PEP compiler with fail-closed constraint compilation
- Plugin discovery via types-registry
- Static dev plugin with tenant scoping (denies on nil/missing tenant)
- ClientHub registration for in-process consumption

### Phase 2: Production PDP Plugin (Planned)

- Advanced predicates: `in_tenant_subtree`, `in_group`, `in_group_subtree`.
- Native `in_group` requires local `resource_group_membership` and `gts_type` tables plus a `ResourceType::with_group_membership_type(...)` mapping. Native `in_group_subtree` additionally requires `resource_group_closure` and advertises both `GroupMembership` and `GroupHierarchy`.
- A service MUST omit a group capability when any required projection is unavailable. Capability omission asks a capable PDP to return expanded resource-ID `in` predicates; degradation is not automatic in the PEP, so a PDP unable to expand must deny.
- Local projection tables for hierarchy-aware constraints (`tenant_closure`, `resource_group_closure`; `resource_group_membership` projection not recommended but not forbidden)
