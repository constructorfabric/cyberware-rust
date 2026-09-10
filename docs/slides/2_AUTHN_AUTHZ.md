---
marp: true
title: "Authentication & Authorization in Constructor Fabric Gears"
description: "Vendor-agnostic AuthN/AuthZ — PDP/PEP, query-level constraints, compile-time safety"
theme: rose-pine-moon
paginate: true
---

<!-- _class: lead -->

# Authentication & Authorization

### in Constructor Fabric Gears (Rust)

Vendor-agnostic identity & access · query-level enforcement · compile-time safety

By the **Constructor Fabric Foundation** · Apache-2.0

June 23, 2026

---

## Agenda

- **The premise** — Gears is a vendor-agnostic framework
- **Foundations** — tenant model & resource groups
- **Abstraction** — plugins, separate AuthN / AuthZ resolvers
- **Authentication** — AuthN Resolver → `SecurityContext`
- **Authorization** — coarse- vs fine-grained · PDP / PEP / PIP
- **The fine-grained model** — AuthZEN + query-level constraints
- **Mechanics** — predicate types, projection tables
- **In code** — `SecurityContext` → `PolicyEnforcer` → `AccessScope` → repo
- **Platform-plane auth**
- **Roadmap** · **Takeaways**

---

## The premise — Gears is vendor-agnostic

Gears is **batteries-included** — many built-in gears you compose into a platform out of the box: tenancy, auth, events, and more.

- The **killer feature**: that same system can also **slot into a vendor's existing platform**
- That vendor almost always **already has** identity and access management
- Their stack varies — OIDC/JWT or opaque tokens · RBAC, ABAC, ReBAC, custom DSLs — Gears assumes **none** of it

> So Gears defines the **contracts**; the vendor supplies the **behavior**.
> No policy-language lock-in, and resources never leave the gear's own database.

---

## Tenant model & tenant isolation

A **tenant** is the boundary of ownership and isolation.

- Every resource must belong to **exactly one** tenant
- **Single-root tree** topology: one root; every other tenant has one parent
- **Isolation by default** — no cross-tenant access; a parent *may* reach its children
- **Barriers** — a `self_managed` tenant hides its subtree from the parent
  (business data is hidden, but **billing / usage still rolls up**)

---

## Resource Group Model

An **optional** layer for grouping resources, so access can be granted at the **group** level
instead of per-resource — and **inherited** down the hierarchy.

| | **Tenant** | **Resource Group** |
|---|---|---|
| Purpose | ownership, isolation, billing | grouping for access control |
| Hierarchy | single-root **tree** | **forest** (roots per tenant) |
| Relationship | ownership (1:N) | membership (M:N) |

> Always **tenant-scoped**: cross-tenant groups are forbidden, and the tenant predicate always applies alongside the group predicate.

---

## Plugin — the main abstraction mechanism

A **plugin** is how Gears stays vendor-neutral — the platform's **open-closed extension point**.

- A **host gear** publishes a plugin **interface** (a trait) in its SDK and registers its schema in the Types Registry
- A **plugin gear** *implements* that interface and registers as a **scoped `ClientHub` client**, keyed by **GTS instance ID**
- The host discovers plugins at runtime and routes to the one a `vendor` config field selects
- **Built-in** → compiled in-process · **External** → separate deployable over gRPC

> Add a new backend (e.g. a custom auth provider) = **write a plugin** —
> the host gear and all its consumers stay unchanged.

---

## Two plugins: AuthN Resolver & AuthZ Resolver

Each half of the story is just a plugin:

- **Authentication** → an **AuthN Resolver Plugin**
- **Authorization** → an **AuthZ Resolver Plugin**

| | **AuthN Resolver** | **AuthZ Resolver** |
|---|---|---|
| Standard | OIDC / JWT | OpenID **AuthZEN** |
| Answers | *Who is the caller?* | *What may they touch?* |
| Output | `SecurityContext` | decision + **constraints** |

> Kept separate on purpose — different standards & caching, credentials isolated in AuthN.
> Mix & match: a standard IdP (Auth0, Okta) **+** a custom policy engine (OpenFGA, Oso).

---

## Authentication → `SecurityContext`

The AuthN Resolver plugin validates the token and produces a `SecurityContext`:

- `subject_id` · `subject_type` · `subject_tenant_id` — *who*, and their home tenant
- `token_scopes` — capability ceiling from the token (first-party `["*"]` vs scoped third-party)
- `bearer_token` — secret-wrapped, never logged; forwarded to the PDP, or reused on the gear's own **out-of-process** calls

The plugin owns: token validation, claim extraction & enrichment, scope detection.

> Gear code **never** parses tokens or resolves tenancy directly — it must stay
> **token-format agnostic**, so that is the AuthN Resolver's job.

---

## The AuthN Resolver interface

A vendor plugin implements **one small trait** — that's the entire integration surface:

```rust
#[async_trait]
pub trait AuthNResolverPluginClient: Send + Sync {
    /// Validate a bearer token → identity.
    async fn authenticate(
        &self,
        bearer_token: &str,
    ) -> Result<AuthenticationResult, AuthNResolverError>;

    /// Service-to-service: OAuth2 client-credentials → identity.
    async fn exchange_client_credentials(
        &self,
        request: &ClientCredentialsRequest,
    ) -> Result<AuthenticationResult, AuthNResolverError>;
}
```

> `AuthenticationResult` carries the `SecurityContext`. Any IdP, any token format —
> JWT, opaque + introspection, PASETO — lives behind these two methods.

---

<!-- _class: lead -->

# Authorization

### Two grains — coarse and fine

---

## Authorization comes in two grains

| | **Coarse-grained** | **Fine-grained** |
|---|---|---|
| Asks | *can this app hit this API at all?* | *which rows may this subject touch?* |
| Unit | OAuth **token scopes** | **permissions** → constraints |
| Where | API Gateway (system middleware) — early reject | the gear's **domain layer** |

> Coarse-grained is a fast app-level **ceiling**; fine-grained is the **row-level** truth —
> and it's where the rest of this talk lives.

---

## Coarse-grained access control

A fast capability gate that runs **before** any policy evaluation:

- **Token scopes** — a per-application ceiling, set by the AuthN plugin
  (first-party UI/CLI → `["*"]` · third-party → `["read:events", …]`)
- **API Gateway route policies** — reject on scope mismatch *before* any fine-grained evaluation
- Coarse, app-level, human-readable — **not** per-resource

```yaml
api-gateway:
  route_policies:
    rules:
      - path: "/events/v1/*"
        required_scopes: ["read:events", "write:events"]  # any of these
```

> A capability ceiling and a fast-path optimization — the per-row, fine-grained checks still run later.

---

## Fine-grained authorization — the vocabulary

Built on the **NIST SP 800-162** PDP / PEP model:

| Role | Responsibility | In Gears |
|------|----------------|----------|
| **PDP** — Policy *Decision* Point | evaluate policy → decision + constraints | **AuthZ Resolver plugin** (vendor) |
| **PEP** — Policy *Enforcement* Point | apply the decision at data access | the **domain gear** |
| **PIP** — Policy *Information* Point | supply attributes for the decision | **Account Management gear** (tenants) · **Resource Group gear** (membership & hierarchy) · or **vendor-side** |
| **PAP** — Policy *Administration* Point | author & manage policies | **Access Management gear** *(planned)* · or **vendor-side** |

---

## Existing solutions & standards

The industry already offers building blocks — we evaluated each:

| Option | Why not |
|--------|---------|
| **AuthZEN** (as-is) | a point-check API — no constraints, so LIST needs iterative fetch/filter |
| **Zanzibar / ReBAC** | O(N) checks and **resource sync** into the policy store |
| **OPA** partial eval | policies must be **Rego** → policy-language lock-in |
| **Gear-level auth** | scatters policy across gears; no PDP/PEP separation |

> **Chosen — AuthZEN + constraint extensions:** standards-based and vendor-neutral, with one
> targeted addition — typed predicates — that the next slides unpack.

---

## AuthZEN — the standard we build on

**OpenID AuthZEN** is the OpenID Foundation's authorization API standard — and our PDP/PEP contract.

- **Standard & vendor-neutral** — one request shape across PDPs; growing ecosystem with interop tests
- **Simple & transport-agnostic** — Access Evaluation API: `subject + action + resource + context → decision`
- **Clean PDP / PEP split** — swap the policy engine without touching gear code
- **Extensible** — `context` is the official extension point, exactly where we add `constraints`

> We take the standard as-is and add one thing — `context.constraints` for query-level enforcement.
> [github.com/openid/authzen](https://github.com/openid/authzen)

---

## The problem — point checks aren't enough

AuthZEN's evaluation API answers *"can subject S do action A on resource R?"*.
That alone breaks down for real CRUD:

- **LIST** → fetch a batch, send to PDP, filter, repeat… cursors invalidate, counts are wrong,
  worst case scans the whole table to return an empty page
- **GET / UPDATE / DELETE** → fetch-then-check wastes a query and opens a **TOCTOU** gap

> We need authorization at the **query level** — as SQL `WHERE` clauses —
> not just point-in-time yes/no decisions.

---

## The AuthZ Resolver interface

The PDP side is just as small — one method the vendor plugin implements:

```rust
#[async_trait]
pub trait AuthZResolverPluginClient: Send + Sync {
    /// Evaluate an authorization request → decision + constraints.
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError>;
}
```

> `evaluate` takes an `EvaluationRequest` and returns an `EvaluationResponse`
> (decision + constraints) — both AuthZEN-shaped. The next two slides show each.

---

## Evaluation request

`get_chat` reads one chat → the `EvaluationRequest` the gear passes to `evaluate(...)`:

```jsonc
{
  "subject":  { "id": "user-uuid",
                "properties": { "tenant_id": "tenant-uuid" } },
  "action":   { "name": "read" },
  "resource": {
    "type": "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
    "id": "chat-uuid"
  },
  "context":  {
    "require_constraints": true,
    "supported_properties": ["owner_tenant_id", "owner_id", "id"]
    // "capabilities": [...] — unlock subtree / group predicates:
    //   "tenant_hierarchy" · "group_membership" · "group_hierarchy"
  }
}
```

---

## Evaluation response — AuthZEN + constraints

The PDP replies with a decision plus optional **`context.constraints`** — typed
**predicates** the PEP compiles straight to SQL.

```json
{ "decision": true,
  "context": { "constraints": [ { "predicates": [
    { "type": "eq", "resource_property": "owner_tenant_id",
      "value": "tenant-uuid" },
    { "type": "eq", "resource_property": "owner_id",
      "value": "user-uuid" }
  ] } ] } }
```

- Predicates within a constraint are **AND**-ed — *this tenant* **and** *owned by me*
- **No resource sync** — the PDP returns predicates, never resource IDs · **O(1)** per query

---

## Capabilities — what the PEP can enforce

The PEP declares which predicate types it can run locally; the PDP returns only those.

| Capability | Enables | Local table |
|---|---|---|
| *(always on)* | `eq`, `in` | — |
| `tenant_hierarchy` | `in_tenant_subtree` | `tenant_closure` |
| `group_membership` | `in_group` | `resource_group_membership` |
| `group_hierarchy` | `in_group_subtree` | `resource_group_closure` + membership |

> **Degradation** — lacking a capability, the PDP expands to explicit `in` IDs, or denies.
> A gear with no projections still works; it just gets simpler predicates.

---

## Predicate types

The PEP declares which it supports; the PDP returns only those.

| Type | Meaning | Compiles to |
|------|---------|-------------|
| `eq` | property equals value | `col = ?` |
| `in` | property in set | `col IN (?, …)` |
| `in_tenant_subtree` | within a tenant subtree | join `tenant_closure` (barrier-aware) |
| `in_group` | direct group membership | join `resource_group_membership` |
| `in_group_subtree` | group + descendants | join group closure + membership |

> Predicates name **properties**, not columns. Unknown type → **fail-closed**.
> **Extensible** — vendors can register custom predicate types.

---

## Projection tables — strategy

Subtree/group predicates need **local** hierarchy data (`tenant_closure`,
`resource_group_membership`, group closure) so the PEP can `JOIN` at query time.

- **Monolith / shared DB** — co-located canonical tables, no projection needed
- **Microservices** — default to PDP-resolved `in` predicates (two-request pattern)
- **Project only after profiling** — membership tables can be ~10× the hierarchy size

> **Replication of these projection tables is _in-design_** — keeping local copies in sync
> across services is the open work item.

---

## Permissions — what can be granted

A **permission** = `{ resource_type, action }`, declared by each gear as a GTS instance of
`gts.cf.toolkit.authz.permission.v1~`:

```json
{ "id": "gts.cf.toolkit.authz.permission.v1~cf.mini_chat._.chat_read.v1",
  "resource_type": "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~",
  "action": "read",
  "display_name": "Read chat" }
```

- `resource_type` — a concrete type, a **wildcard**, or an **ABAC query** (`…[category='support']`)
- `action` — one concrete verb; the catalog is discoverable by admin UIs / the Access Management gear

> Scopes and permissions compose: `effective_access = min(token_scopes, user_permissions)`

---

## Request lifecycle

<style scoped>
section { display: flex; flex-direction: column; justify-content: center; }
</style>

![h:560](../img/request_sequence.png)

---

<!-- _class: lead -->

# Authorization in code

### `SecurityContext` → `PolicyEnforcer` → `AccessScope` → repository

---

## Domain layer — the read path (`get_chat`)

```rust
pub async fn get_chat(&self, ctx: &SecurityContext, id: Uuid)
    -> Result<ChatDetail, DomainError>
{
    let conn = self.db.conn()?;

    // PEP: ask the PDP what this subject may read
    let chat_scope = self.enforcer
        .access_scope(ctx, &resources::CHAT, actions::READ, Some(id))
        .await?                            // → AccessScope (row-level constraints)
        .ensure_owner(ctx.subject_id());   // defense-in-depth

    // the scope flows into the repository as the WHERE clause
    let chat = self.chat_repo.get(&conn, &chat_scope, id).await?
        .ok_or_else(|| DomainError::chat_not_found(id))?;
    Ok(/* … */)
}
```

---

## Domain layer — the write path (`create_chat`)

```rust
let scope = self.enforcer
    .access_scope_with(
        ctx, &resources::CHAT, actions::CREATE, None,
        &AccessRequest::new()                        // declare the proposed owner
            .resource_property(pep_properties::OWNER_TENANT_ID, tenant_id)
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id()),
    )
    .await?;

let created = self.chat_repo.create(&conn, &scope, chat).await?;
```

> For **CREATE** there is no row yet — the gear sends the *proposed* owner properties so the
> PDP can authorize the insert. Same `AccessScope`, same repository contract.

---

## Infra layer — secure by construction

**Entity** opts into scoping; the macro forces an explicit column mapping:

```rust
#[derive(DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chats")]
#[secure(tenant_col = "tenant_id", owner_col = "user_id",
         resource_col = "id", no_type)]
pub struct Model { /* id, tenant_id, user_id, model, … */ }
```

**Repository** turns the `AccessScope` into a `WHERE` clause:

```rust
Entity::find()
    .filter(/* id = ? AND deleted_at IS NULL */)
    .secure()            // opt into scoped query
    .scope_with(scope)   // ← AccessScope becomes the WHERE
    .one(conn).await?
```

---

## Forget the scope? It won't compile

`.secure()` returns an **`Unscoped`** query — the execution methods exist only once it is scoped:

```rust
Entity::find()
    .secure()            // SecureSelect<Entity, Unscoped>
    .all(conn).await     // ← forgot .scope_with(scope)
```

```text
error[E0599]: no method named `all` found for struct
  `SecureSelect<Entity, Unscoped>` in the current scope
   = note: the method was found for `SecureSelect<E, Scoped>`
```

> A **typestate**, not a lint — an unscoped query has no `.all()` / `.one()` to call.
> A maintained `trybuild` test keeps this guarantee from regressing.

---

## The type system enforces tenancy

Forget to decide the tenant mapping — it **does not compile**:

```rust
#[derive(Scopable)]
#[secure(
    resource_col = "id",
    no_owner,
    no_type,
)]                       // ← no tenant_col and no no_tenant
struct Model;
```

```text
error: secure: missing explicit decision for tenant:
         use `tenant_col = "column_name"` or `no_tenant`
 --> src/infra/db/entity/chat.rs
```

> Security isn't a convention you can forget — an unscoped entity is a **build failure**.

---

## Platform-plane authentication (out-of-process)

When gears call each other **outside** a user request (registration, heartbeats, jobs),
how does a gear prove *its own* identity?

- **Phase 1 (now)** — K8s ServiceAccount tokens (`TokenReview`) · bootstrap token over UDS
- **End state** — **mTLS + SPIFFE** workload certs (`spiffe://…/gear/<gear>/<version>`)
- One abstraction across phases: `InternalCredential` + `PlatformIdentity`
- Distinct header **`X-ToolKit-Internal-Token`** — never collides with the user's `Authorization`

> Tenant-plane (user JWT) and platform-plane (gear identity) are **separate trust planes**.

---

## Not done yet — roadmap

Being honest about the gaps; each has a clear path:

- **Batch evaluation optimization** — one `evaluate` call for many resources at once
- **Local projections sync** — keep `tenant_closure` & membership replicas in step
- **Authorization decision caching** — cache PDP decisions + constraints (TTL-bounded)
- **Multi-Factor Authentication (MFA)** — step-up / assurance-level awareness in AuthN
- **S2S `SecurityContext` caching** — reuse client-credentials identities
- **More architecture lints** — widen compile-time architecture enforcement
- **Access Management built-in gear** — policy administration (PAP) + a default PDP

> Mostly performance, coverage, and tooling — the core model is already in place.

---

## Takeaways

1. **Vendor-agnostic by design** — host gears define contracts; vendor plugins supply
   authn & authz. No policy lock-in, no resource sync.
2. **Query-level enforcement** — the PDP returns predicates, the PEP compiles them to SQL.
   Correct LIST, pagination, counts, and TOCTOU safety — at O(1) overhead.
3. **Compile-time safety** — `SecurityContext` → `PolicyEnforcer` → `AccessScope` → scoped
   repo, and an unscoped entity simply **won't build**.

> One coherent security data-path — with **no unscoped shortcut** to reach for.

---

<!-- _class: lead -->

# Questions?

### Authentication & Authorization in Constructor Fabric Gears

*Vendor-agnostic · Query-level enforcement · Compile-time safe*

`docs/arch/authorization/` · `docs/arch/toolkit-oop/`