# Security: AuthN, AuthZ & Secure Database Access

This document describes the complete security data-path in Gears: how requests are authenticated (AuthN), how authorization decisions are enforced (AuthZ / PEP), how `AccessScope` is produced, and how the Secure ORM layer (`SecureConn`) applies it to every database query.

For the full architectural design (AuthZEN model, predicate types, caching, deployment modes), see [`docs/arch/authorization/DESIGN.md`](../arch/authorization/DESIGN.md).

## Core invariants

- **Rule**: Authentication is handled by API Gateway middleware — gears receive a ready-to-use `SecurityContext`.
- **Rule**: Use `SecureConn` for all DB access in handlers/services. Gears cannot access raw database connections/pools.
- **Rule**: Derive `Scopable` on SeaORM entities with tenant/resource columns.
- **Rule**: Use `PolicyEnforcer` from `authz-resolver-sdk` for all authorization decisions. Do not construct `AccessScope` manually in production code.
- **Rule**: Every sensitive DB access MUST be covered by a PDP decision (via `PolicyEnforcer`). Exception: the approved prefetch-first flow for GET/UPDATE/DELETE may read with `AccessScope::allow_all()` before the PDP call, provided the required compensating checks are applied (see [GET prefetch pattern](#get--prefetch-pattern) and [UPDATE/DELETE prefetch + TOCTOU safety](#update--delete--prefetch--toctou-safety)).
- **Rule**: Fail-closed — denied PDP decisions, unreachable PDP, and missing constraints all result in 403 Forbidden.
- **Rule**: Map `EnforcerError` to domain errors. Never expose PDP internals to the client.
- **Rule**: Use `.authenticated()` on `OperationBuilder` for protected endpoints, `.anonymous()` for unauthenticated ones.
- **Rule**: No plain SQL in handlers/services/repos. Raw SQL is allowed only in migration infrastructure. See [`11_database_patterns.md`](./11_database_patterns.md) for migration rules.

## Architecture overview

```text
Request → API Gateway (AuthN middleware) → SecurityContext
              ↓
         Gear Handler (PEP)
              ↓
         PolicyEnforcer → AuthZ Resolver (PDP) → decision + constraints
              ↓
         PEP Compiler → AccessScope
              ↓
         SecureConn (SQL WHERE) → Database
```

Three components work together:

1. **AuthN Resolver** — validates bearer tokens, produces `SecurityContext` (subject identity, tenant, token scopes). Uses the gateway + plugin pattern to delegate to vendor-specific IdPs.
2. **AuthZ Resolver (PDP)** — evaluates policies, returns `decision + constraints`. Uses the gateway + plugin pattern to delegate to vendor-specific authorization services.
3. **Domain gears (PEP)** — call PDP via `PolicyEnforcer`, compile constraints to `AccessScope`, pass to `SecureConn` for SQL-level enforcement.

Gear developers interact primarily with the PEP layer — the AuthN and AuthZ resolvers are infrastructure gears.

## AuthN: how requests get authenticated

### Route policies

When registering REST endpoints, declare whether each route requires authentication using the type-state builder pattern:

```rust
// Protected endpoint — requires valid bearer token + license check
OperationBuilder::get("/users-info/v1/users")
    .authenticated()
    .require_license_features::<License>([])
    .handler(handlers::list_users)
    // ...

// Public endpoint — no token required
OperationBuilder::get("/users-info/v1/health")
    .anonymous()
    .handler(handlers::health)
    // ...
```

The builder enforces at compile time that every route declares its auth posture before `.register()`:
- `.authenticated()` marks the route as protected, then requires `.require_license_features::<L>(features)` (or `.no_license_required()`) before registration.
- `.anonymous()` marks the route as unauthenticated and automatically satisfies the license requirement.

The API Gateway middleware uses these declarations to decide how to handle each request:
- **Protected routes**: extract bearer token → call AuthN Resolver → inject `SecurityContext` into request extensions. Returns 401 if token is invalid.
- **Public routes**: inject `SecurityContext::anonymous()` (zero-UUID subject and tenant, empty scopes).
- **Unregistered routes**: behavior depends on `require_auth_by_default` config flag.

### Extracting SecurityContext in handlers

The API Gateway injects `SecurityContext` as an Axum `Extension`. Extract it with the standard `Extension` extractor:

```rust
use toolkit::api::prelude::*;
use toolkit_security::SecurityContext;

pub async fn list_users(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    OData(query): OData,
) -> ApiResult<JsonPage<serde_json::Value>> {
    let page = svc.users.list_users_page(&ctx, &query).await?;
    Ok(Json(page))
}
```

### SecurityContext fields

| Field | Required | Description |
|-------|----------|-------------|
| `subject_id` | Yes | Unique subject identifier (from token `sub` claim) |
| `subject_tenant_id` | Yes | Tenant the subject belongs to |
| `subject_type` | No | GTS type identifier (e.g., `gts.cf.core.security.subject_user.v1~`) |
| `token_scopes` | Yes | Capability restrictions (`["*"]` for first-party apps) |
| `bearer_token` | No | Original token (wrapped in `Secret<String>`, forwarded to PDP) |

## AuthZ: PolicyEnforcer (PEP)

### Wiring up

1. **Declare dependency** on `authz-resolver` in your gear:

```rust
#[toolkit::gear(
    name = "my_gear",
    deps = ["authz-resolver"],
    capabilities = [db, rest],
)]
pub struct MyGear { /* ... */ }
```

2. **Resolve the AuthZ client** from `ClientHub` during `init()`:

```rust
use authz_resolver_sdk::AuthZResolverApi;

async fn init(&self, ctx: &GearCtx) -> anyhow::Result<()> {
    let authz = ctx.client_hub().get::<dyn AuthZResolverApi>()?;
    // Pass authz to your domain service...
}
```

3. **Create a single `PolicyEnforcer`** in your service and clone it into sub-services:

```rust
use authz_resolver_sdk::PolicyEnforcer;

let enforcer = PolicyEnforcer::new(authz);
// Clone enforcer into each sub-service (cheap Arc clone)
```

### Defining resource types

Each resource type declares which properties the PEP can compile from PDP constraints into SQL:

```rust
use authz_resolver_sdk::pep::ResourceType;
use toolkit_security::pep_properties;

pub const USER: ResourceType = ResourceType::from_static(
    "my_gear.user",
    &[
        pep_properties::OWNER_TENANT_ID,  // tenant scoping
        pep_properties::RESOURCE_ID,      // resource-level access
    ],
);

pub const DOCUMENT: ResourceType = ResourceType::from_static(
    "my_gear.document",
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::RESOURCE_ID,
        pep_properties::OWNER_ID,  // ownership-based access
        "category_id",            // custom domain property
    ],
);
```

Well-known properties from `toolkit_security::pep_properties`:
- `OWNER_TENANT_ID` — tenant that owns the resource
- `RESOURCE_ID` — the resource's primary key
- `OWNER_ID` — the user who owns the resource

A resource that can execute native `InGroup`/`InGroupSubtree` predicates must
also declare the RG member-handle type used in
`resource_group_membership.gts_type_id`:

```rust
pub const USER: ResourceType = ResourceType::from_static(
    "my_gear.user",
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
)
.with_group_membership_type("gts.cf.core.rg.type.v1~example.core.mygear.user.v1~");
```

This value is not inferred from the AuthZ resource name: gears may use different
GTS paths for policy matching and RG membership storage. `PolicyEnforcer`
suppresses `GroupMembership` and `GroupHierarchy` capabilities for resource
descriptors without this mapping, asking the PDP to expand the group scope to
explicit resource-ID `in` predicates; a PDP that cannot expand must deny (the
bundled plugins deny — neither implements expansion today). The querying database must also contain or project the tables each
predicate executes against before the enforcer is configured with the
corresponding capability: native `in_group` needs `resource_group_membership`
and RG's `gts_type` registry; native `in_group_subtree` additionally needs
`resource_group_closure`.

### Defining actions

Define action constants used in PDP requests:

```rust
pub const GET: &str = "get";
pub const LIST: &str = "list";
pub const CREATE: &str = "create";
pub const UPDATE: &str = "update";
pub const DELETE: &str = "delete";
```

### PolicyEnforcer API

Two entry points:

```rust
// Simple: always requires constraints from PDP
let scope = enforcer
    .access_scope(&ctx, &resource_type, action, resource_id)
    .await?;

// Advanced: per-request overrides via AccessRequest
let scope = enforcer
    .access_scope_with(&ctx, &resource_type, action, resource_id, &access_request)
    .await?;
```

Both return `Result<AccessScope, EnforcerError>`. The `AccessScope` is then passed to `SecureConn` methods for SQL-level enforcement.

### AccessRequest builder

Use `AccessRequest` for per-call overrides:

```rust
use authz_resolver_sdk::pep::AccessRequest;
use toolkit_security::pep_properties;

let request = AccessRequest::new()
    // Pass resource properties to PDP for narrow constraints
    .resource_property(pep_properties::OWNER_TENANT_ID, tenant_id)
    // Override constraint requirement (default: true)
    .require_constraints(false)
    // Override tenant context
    .context_tenant_id(specific_tenant_id);
```

## How AuthZ connects to the database: pep_prop

The bridge between PDP authorization constraints and SQL `WHERE` clauses is **property resolution** — mapping abstract PEP property names (like `"owner_tenant_id"`) to concrete database columns (like `Column::TenantId`).

### Scopable entity attributes

Every entity that participates in row-level security derives `Scopable` with a `#[secure(...)]` attribute declaring its security dimensions:

```rust
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "users")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "id",
    no_owner,
    no_type
)]
pub struct Model { /* ... */ }
```

Available dimension attributes:

| Attribute | PEP property auto-mapped | Description |
|-----------|--------------------------|-------------|
| `tenant_col = "col"` | `owner_tenant_id` → `Column::Col` | Tenant ownership column |
| `resource_col = "col"` | `id` → `Column::Col` | Resource identity (usually PK) |
| `owner_col = "col"` | `owner_id` → `Column::Col` | User ownership column |
| `type_col = "col"` / `no_type` | *(no auto-map)* | GTS type column |
| `no_tenant` / `no_resource` / `no_owner` | *(dimension absent)* | Opt out of a dimension |
| `unrestricted` | *(no scoping at all)* | Global tables without scoping columns |

**Rule**: All four dimensions must be declared (either `*_col` or `no_*`), unless `unrestricted` is used.

### Standard property auto-mapping

The `Scopable` derive macro automatically maps dimension columns to well-known PEP property names via `resolve_property()`:

| Dimension attribute | PEP property name | Constant |
|---------------------|-------------------|----------|
| `tenant_col = "tenant_id"` | `"owner_tenant_id"` | `pep_properties::OWNER_TENANT_ID` |
| `resource_col = "id"` | `"id"` | `pep_properties::RESOURCE_ID` |
| `owner_col = "user_id"` | `"owner_id"` | `pep_properties::OWNER_ID` |

This means if you declare `tenant_col = "tenant_id"`, the macro generates:

```rust
fn resolve_property(property: &str) -> Option<Self::Column> {
    match property {
        "owner_tenant_id" => Some(Column::TenantId),
        "id"              => Some(Column::Id),
        // ...
        _ => None,
    }
}
```

When the PDP returns a constraint like `In("owner_tenant_id", [uuid1, uuid2])`, `SecureConn` calls `resolve_property("owner_tenant_id")`, gets `Column::TenantId`, and generates `WHERE tenant_id IN (uuid1, uuid2)`.

### Custom properties with `pep_prop`

For domain-specific properties beyond the three standard ones, use `pep_prop(property = "column")`:

```rust
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "addresses")]
#[secure(
    tenant_col = "tenant_id",
    resource_col = "id",
    owner_col = "user_id",
    no_type,
    pep_prop(city_id = "city_id")
)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub city_id: Uuid,
    pub street: String,
    // ...
}
```

This adds `"city_id" => Some(Column::CityId)` to the generated `resolve_property()` match.

To use the custom property, include it in both `ResourceType.supported_properties` and the PDP policy:

```rust
pub const ADDRESS: ResourceType = ResourceType {
    name: "my_gear.address",
    supported_properties: &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::RESOURCE_ID,
        pep_properties::OWNER_ID,
        "city_id",  // matches the pep_prop key
    ],
};
```

When the PDP returns constraints referencing `"city_id"`, the PEP compiler includes them in the `AccessScope`, and `SecureConn` resolves them to `Column::CityId` for the SQL `WHERE` clause.

### The full data flow

```text
ResourceType.supported_properties  ← declares what PEP can compile
         ↓
PDP constraint: In("city_id", [uuid]) ← PDP returns constraint on this property
         ↓
PEP compiler: ScopeFilter::In("city_id", [uuid]) → AccessScope
         ↓
SecureConn → build_scope_condition::<E>() → E::resolve_property("city_id")
         ↓
Column::CityId → WHERE city_id IN (uuid)
```

If `resolve_property()` returns `None` for a property (unknown property), the entire constraint fails (fail-closed → deny-all).

### pep_prop validation rules

The macro enforces at compile time:
- **Reserved names rejected** — `owner_tenant_id`, `id`, `owner_id` cannot be used in `pep_prop()` (use `tenant_col`, `resource_col`, `owner_col` instead).
- **No duplicates** — each property name may appear only once.
- **Empty names rejected** — property and column names must be non-empty.
- **Forbidden with `unrestricted`** — `pep_prop()` cannot be combined with `unrestricted`.

### Unrestricted entities (`#[secure(unrestricted)]`)

Use `#[secure(unrestricted)]` only for truly global tables where the entity has **no scoping columns**. Notes:

- `secure_insert` does not require `tenant_id` for such entities.
- Queries with a scope that contains tenant IDs will be denied (by policy: tenants requested but entity has no `tenant_col`).
- If you need to read/write a global table within a tenant-scoped request, do not use `unrestricted`. Model it with explicit columns and use an appropriate scope shape (often `resources_only`).

## CRUD authorization patterns

All patterns below are from the canonical `users-info` example gear.

### LIST — simple scope

The simplest case: no resource ID, constraints required by default.

```rust
pub async fn list_users_page(
    &self,
    ctx: &SecurityContext,
    query: &ODataQuery,
) -> Result<Page<User>, DomainError> {
    let conn = self.db.conn()?;

    // PDP returns constraints → compiled to AccessScope
    let scope = self.policy_enforcer
        .access_scope(ctx, &resources::USER, actions::LIST, None)
        .await?;

    // SecureConn applies scope as SQL WHERE clause
    let page = self.repo.list_page(&conn, &scope, query).await?;
    Ok(page)
}
```

### GET — prefetch pattern

For point reads, a two-step pattern provides narrower PDP constraints:

```rust
pub async fn get_user(
    &self,
    ctx: &SecurityContext,
    id: Uuid,
) -> Result<User, DomainError> {
    let conn = self.db.conn()?;

    // Step 1: Prefetch with allow_all to extract owner_tenant_id
    let prefetch_scope = AccessScope::allow_all();
    let user = self.repo
        .get(&conn, &prefetch_scope, id)
        .await?
        .ok_or_else(|| DomainError::user_not_found(id))?;

    // Step 2: PDP call with prefetched properties → narrow constraint
    let scope = self.policy_enforcer
        .access_scope_with(
            ctx,
            &resources::USER,
            actions::GET,
            Some(id),
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, user.tenant_id)
                .require_constraints(false),
        )
        .await?;

    // Step 3: If unconstrained, return prefetch; otherwise scoped re-read
    let user = if scope.is_unconstrained() {
        user
    } else {
        self.repo
            .get(&conn, &scope, id)
            .await?
            .ok_or_else(|| DomainError::user_not_found(id))?
    };

    Ok(user)
}
```

**Why prefetch?** Without it, PDP would need to expand the full tenant subtree. By providing `owner_tenant_id`, PDP can return a simple `eq` predicate. If PDP returns `decision: true` without constraints (unconstrained), we skip the second DB query entirely.

### CREATE — resource properties

CREATE operations pass the target `owner_tenant_id` as a resource property so the PDP can validate the subject is allowed to create in that tenant:

```rust
pub async fn create_user(
    &self,
    ctx: &SecurityContext,
    new_user: NewUser,
) -> Result<User, DomainError> {
    let conn = self.db.conn()?;

    // PDP validates: can subject create in this tenant?
    let scope = self.policy_enforcer
        .access_scope_with(
            ctx,
            &resources::USER,
            actions::CREATE,
            None,
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, new_user.tenant_id),
        )
        .await?;

    // SecureConn INSERT validates tenant_id is within the scope
    let created = self.repo.create(&conn, &scope, user).await?;
    Ok(created)
}
```

### UPDATE / DELETE — prefetch + TOCTOU safety

Mutations combine prefetch (for narrow PDP constraints) with scoped writes (for TOCTOU protection):

```rust
pub async fn update_user(
    &self,
    ctx: &SecurityContext,
    id: Uuid,
    patch: UserPatch,
) -> Result<User, DomainError> {
    let conn = self.db.conn()?;

    // Step 1: Prefetch to extract owner_tenant_id
    let prefetch_scope = AccessScope::allow_all();
    let mut current = self.repo
        .get(&conn, &prefetch_scope, id)
        .await?
        .ok_or_else(|| DomainError::user_not_found(id))?;

    // Step 2: PDP call with prefetched properties
    let scope = self.policy_enforcer
        .access_scope_with(
            ctx,
            &resources::USER,
            actions::UPDATE,
            Some(id),
            &AccessRequest::new()
                .resource_property(pep_properties::OWNER_TENANT_ID, current.tenant_id),
        )
        .await?;

    // Step 3: Apply patch
    if let Some(email) = patch.email { current.email = email; }
    if let Some(name) = patch.display_name { current.display_name = name; }

    // Step 4: Scoped write — WHERE clause enforces TOCTOU safety
    let updated = self.repo.update(&conn, &scope, current).await?;
    Ok(updated)
}
```

**TOCTOU protection**: between prefetch and mutation, the resource's tenant might change (race condition). The scoped write includes `WHERE (scope constraints)`, so if the tenant changed, the update returns 0 rows → treated as not found.

## SecureConn usage

### Preferred: SecureConn for scoped access

```rust
use toolkit_db::secure::AccessScope;

pub async fn list_users(
    Extension(ctx): Extension<SecurityContext>,
    Extension(db): Extension<Arc<DbHandle>>,
) -> ApiResult<JsonPage<UserDto>> {
    let secure_conn = db.sea_secure();
    // In production, scope comes from PolicyEnforcer (see CRUD patterns above)
    let scope = enforcer.access_scope(&ctx, &resources::USER, actions::LIST, None).await?;
    let users = secure_conn
        .find::<user::Entity>(&scope)
        .all(&secure_conn)
        .await?;
    Ok(Json(users.into_iter().map(UserDto::from).collect()))
}
```

### Implicit security policy (how `AccessScope` becomes SQL)

| Scope | Entity has column? | Result |
|------|---------------------|--------|
| Empty (`deny_all`) | N/A | deny all (`WHERE false`) |
| Unconstrained (`allow_all`) | N/A | no filtering (`WHERE true`) |
| Tenants only | has `tenant_col` | `tenant_col IN (tenant_ids)` |
| Tenants only | no `tenant_col` | deny all |
| Resources only | has `resource_col` | `resource_col IN (resource_ids)` |
| Resources only | no `resource_col` | deny all |
| Tenants + resources | has both | AND them |
| Tenants + resources | missing either column | deny all |

This is enforced inside `toolkit-db` when you call `.scope_with(&scope)` / `SecureConn::find*` / `SecureConn::update_many` / `SecureConn::delete_many`.

### OR/AND semantics

- Multiple constraints are OR-ed (alternative access paths)
- Filters within a constraint are AND-ed (all must match)
- Unknown properties fail that constraint (fail-closed)
- If all constraints fail resolution → deny-all

### Auto-scoped queries

```rust
let secure_conn = db.sea_secure();

// Automatically adds scope filters
let users = secure_conn
    .find::<user::Entity>(&scope)
    .all(&secure_conn)
    .await?;

// Automatically adds scope + id filter
let user = secure_conn
    .find_by_id::<user::Entity>(&scope, user_id)?
    .one(&secure_conn)
    .await?;
```

### Manual scoping

```rust
use toolkit_db::secure::SecureEntityExt;

// For complex queries, build your filters first, then apply scope and execute via SecureConn.
let user = user::Entity::find()
    .filter(user::Column::Email.eq(email))
    .secure()
    .scope_with(&scope)
    .one(&secure_conn)
    .await?;
```

### Advanced scoping for joins / related entities

Use these when the base entity cannot be tenant-filtered directly:

- `SecureSelect::and_scope_for::<J>(&scope)` — apply tenant scoping on a joined entity `J`.
- `SecureSelect::scope_via_exists::<J>(&scope)` — apply tenant scoping via an `EXISTS` subquery on `J`.

## Mutations (security rules)

### Insert (`secure_insert` / `SecureConn::insert`)

- If the entity has a `tenant_col`, the `ActiveModel` MUST include `tenant_id`.
- The inserted `tenant_id` MUST be inside `scope.all_values_for(pep_properties::OWNER_TENANT_ID)`.
- Violations are errors (`Denied` / `TenantNotInScope` / `Invalid("tenant_id is required")`).

### Update one record (`SecureConn::update_with_ctx`)

- There is no public unscoped update-one API.
- `update_with_ctx(scope, id, am)` first checks the row exists in scope.
- For tenant-scoped entities, `tenant_id` is immutable. Attempts to change it are denied.

### Update many (`SecureConn::update_many`)

- Must be scoped via `scope_with` / `SecureConn::update_many(scope)`.
- Attempts to set the `tenant_id` column are denied at runtime (`Denied("tenant_id is immutable")`).

## Executors, transactions, repository pattern, and migrations

See [`11_database_patterns.md`](./11_database_patterns.md) for `DBRunner`/`SecureTx`, transaction patterns (`in_transaction_mapped`), the repository pattern, and database migrations.

## Error handling

### EnforcerError (AuthZ errors)

Map `EnforcerError` to your domain error type:

```rust
use authz_resolver_sdk::EnforcerError;

impl From<EnforcerError> for DomainError {
    fn from(e: EnforcerError) -> Self {
        tracing::error!(error = %e, "AuthZ scope resolution failed");
        match e {
            // PDP denied access or constraints failed to compile → 403
            EnforcerError::Denied { .. }
            | EnforcerError::CompileFailed(_) => Self::Forbidden,
            // PDP unreachable or returned invalid response → 500
            EnforcerError::EvaluationFailed(_) => Self::InternalError,
        }
    }
}
```

This mapping follows the fail-closed principle: denial and compilation failures are access errors (403), while evaluation failures are infrastructure errors (500).

### ScopeError (ORM errors)

`ScopeError` is returned by `SecureConn` / `SecureTx` methods when scope violations occur (e.g., inserting into a tenant not in scope, attempting to change `tenant_id`). Map these to appropriate domain errors (typically 403 for denied, 500 for DB errors).

## Development setup

### Static plugins

For development and testing, Gears provide static AuthN and AuthZ plugins:

- **Static AuthN plugin** — accepts all tokens or maps configured tokens to identities. Modes:
  - `accept_all`: any non-empty token maps to a default identity
  - `static_tokens`: specific tokens map to specific identities (subject_id, tenant_id, scopes)

- **Static AuthZ plugin** — returns `decision: true` with an `In` predicate on `owner_tenant_id` scoped to the subject's tenant from the request context. Denies if no tenant is resolvable.

### Feature flags

The server binary uses feature flags to include static plugins:

```bash
# Run with static AuthN + AuthZ plugins (development mode)
cargo run --bin cf-gears-server --features static-authn,static-authz -- --config config.yaml run

# Makefile target includes these features
make example
```

### Config: auth_disabled

For the simplest local development (no auth at all), configure the API Gateway:

```yaml
gears:
  api-gateway:
    auth_disabled: true
```

This injects a default `SecurityContext` for all requests without calling any AuthN resolver.

**Important**: `auth_disabled` only skips AuthN. If your gear depends on `authz-resolver`, the AuthZ call still happens (using the default `SecurityContext` from the gateway). Use static AuthZ plugin to provide predictable authorization responses.

## Testing with SecureConn

### Test setup

```rust
use toolkit_db::DbHandle;
use toolkit_security::AccessScope;

#[tokio::test]
async fn test_user_repository() {
    let db = setup_test_db().await;
    let scope = AccessScope::for_tenant(Uuid::new_v4());
    let repo = UserRepository;
    let conn = db.sea_secure();

    // Test operations
    let user = repo.create(&conn, &scope, new_user).await.unwrap();
    let found = repo.find_by_id(&conn, &scope, user.id).await.unwrap();
    assert_eq!(found.id, user.id);
}
```

In tests, build scopes explicitly (`AccessScope::for_tenant(...)`, `AccessScope::for_tenants(...)`, `AccessScope::for_resources(...)`). This is the one place where manual `AccessScope` construction is appropriate.

## Quick checklist

### AuthZ wiring
- [ ] Add `deps = ["authz-resolver"]` to your gear declaration.
- [ ] Resolve `AuthZResolverApi` from `ClientHub` in `init()`.
- [ ] Create `PolicyEnforcer::new(authz)` once, clone into sub-services.
- [ ] Define `ResourceType` constants with `supported_properties` for each resource.
- [ ] Define action constants (`get`, `list`, `create`, `update`, `delete`).
- [ ] Call `policy_enforcer.access_scope()` or `.access_scope_with()` in every service method.
- [ ] Implement `From<EnforcerError>` for your domain error type.
- [ ] Use `.authenticated()` + `.require_license_features::<License>([])` on protected `OperationBuilder` routes.
- [ ] Use `.anonymous()` only for truly unauthenticated routes (health checks, OpenAPI spec).

### SecureConn / database
- [ ] Derive `Scopable` on SeaORM entities with `tenant_col` (required).
- [ ] Declare all four dimensions or use `unrestricted`.
- [ ] Add `pep_prop(name = "column")` for custom domain properties.
- [ ] Use `db.sea_secure()` for all DB access in handlers/services.
- [ ] Pass the `AccessScope` from `PolicyEnforcer` to `SecureConn` methods.
- [ ] Use `secure_conn.find::<Entity>(&scope).all(&secure_conn)` for auto-scoped queries.
- [ ] Use `secure_conn.update_with_ctx::<Entity>(&scope, id, am)` for single-record updates.
- [ ] See [`11_database_patterns.md`](./11_database_patterns.md) for DBRunner, transaction, repository, and migration checklists.

### CRUD patterns
- [ ] Use prefetch pattern for GET/UPDATE/DELETE (extract `owner_tenant_id` for narrow PDP constraints).
- [ ] For CREATE, pass `owner_tenant_id` as a resource property.
- [ ] In tests, build scopes explicitly.

## Related docs

- Full authorization architecture: [`docs/arch/authorization/DESIGN.md`](../arch/authorization/DESIGN.md)
- Usage scenarios: [`docs/arch/authorization/AUTHZ_USAGE_SCENARIOS.md`](../arch/authorization/AUTHZ_USAGE_SCENARIOS.md)
- Database execution patterns (DBRunner, transactions, repos, migrations): [`11_database_patterns.md`](./11_database_patterns.md)
- REST OperationBuilder (`.authenticated()`, `.anonymous()`): [`04_rest_operation_builder.md`](./04_rest_operation_builder.md)
- OData pagination / filtering: [`07_odata_pagination_select_filter.md`](./07_odata_pagination_select_filter.md)
- Canonical example: `examples/toolkit/users-info/users-info/src/`
