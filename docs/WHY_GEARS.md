# Why Should I Use Constructor Fabric Gears (Rust)?

<!-- toc -->

- [Executive Summary](#executive-summary)
  - [At a glance: Go vs C# vs Rust vs Rust + Gears](#at-a-glance-go-vs-c-vs-rust-vs-rust--gears)
- [Part A — Where Rust has advantages over Go and C# (for platform code)](#part-a--where-rust-has-advantages-over-go-and-c-for-platform-code)
  - [A.1 Errors are part of the type system](#a1-errors-are-part-of-the-type-system)
  - [A.2 Memory data races are a compile error, not a `-race` flag](#a2-memory-data-races-are-a-compile-error-not-a--race-flag)
  - [A.3 No `nil` interfaces, no exceptions-from-anywhere](#a3-no-nil-interfaces-no-exceptions-from-anywhere)
  - [A.4 Sum types make illegal states unrepresentable](#a4-sum-types-make-illegal-states-unrepresentable)
  - [A.5 Exhaustive `match` makes state evolution safer](#a5-exhaustive-match-makes-state-evolution-safer)
  - [A.6 Zero-cost abstractions and predictable performance](#a6-zero-cost-abstractions-and-predictable-performance)
  - [A.7 Tooling and static analysis as a first-class citizen](#a7-tooling-and-static-analysis-as-a-first-class-citizen)
  - [A.8 Newtypes make identity mix-ups a compile error](#a8-newtypes-make-identity-mix-ups-a-compile-error)
  - [A.9 RAII, lifetimes, and scoped resources](#a9-raii-lifetimes-and-scoped-resources)
  - [A.10 Traits and generics: zero-cost polymorphism](#a10-traits-and-generics-zero-cost-polymorphism)
  - [A.11 Macros move framework rules into compile time](#a11-macros-move-framework-rules-into-compile-time)
- [Part B — Why "just Rust" is not enough: what Gears adds](#part-b--why-just-rust-is-not-enough-what-gears-adds)
  - [B.1 A pre-integrated configurable XaaS backbone](#b1-a-pre-integrated-configurable-xaas-backbone)
  - [B.2 Spec-driven development with Studio](#b2-spec-driven-development-with-studio)
  - [B.3 Tenant isolation by default](#b3-tenant-isolation-by-default)
  - [B.4 Authentication & authorization, built in (NIST SP 800-162 PDP/PEP)](#b4-authentication--authorization-built-in-nist-sp-800-162-pdppep)
  - [B.5 Prewritten architecture lints (`cargo gears lint`)](#b5-prewritten-architecture-lints-cargo-gears-lint)
  - [B.6 Runtime Gears capabilities](#b6-runtime-gears-capabilities)
  - [B.7 One consistent API dialect: `OperationBuilder` + OpenAPI + OData](#b7-one-consistent-api-dialect-operationbuilder--openapi--odata)
  - [B.8 Composable gears: one codebase, many deployment shapes](#b8-composable-gears-one-codebase-many-deployment-shapes)
  - [B.9 Extensible domain model via the Global Type System (GTS)](#b9-extensible-domain-model-via-the-global-type-system-gts)
  - [B.10 Canonical errors](#b10-canonical-errors)
  - [B.11 Observability and operational defaults](#b11-observability-and-operational-defaults)
  - [B.12 FIPS 140-3 support](#b12-fips-140-3-support)
  - [B.13 Supply-chain policy as code](#b13-supply-chain-policy-as-code)
  - [B.14 Preconfigured build-gated safety](#b14-preconfigured-build-gated-safety)
  - [B.15 Local-first, shift-left development](#b15-local-first-shift-left-development)
- [When Gears is (and isn't) the right choice](#when-gears-is-and-isnt-the-right-choice)
- [Get started](#get-started)

<!-- /toc -->

> A guide for **Go developers** (and C# developers) evaluating [Constructor Fabric Gears](https://github.com/constructorfabric/gears-rust) — a secure, modular **XaaS development framework & middleware** written in Rust.

**Public links**

- **Gears (Rust) monorepo** — <https://github.com/constructorfabric/gears-rust>
- **Architecture Manifest** — [`docs/ARCHITECTURE_MANIFEST.md`](./ARCHITECTURE_MANIFEST.md)
- **Overview slides** — [`docs/slides/1_OVERVIEW.md`](./slides/1_OVERVIEW.md)
- **Gears inventory** — [`docs/GEARS.md`](./GEARS.md)
- **Toolkit guide** — [`docs/toolkit_unified_system/README.md`](./toolkit_unified_system/README.md)
- **Global Type System (GTS)** — <https://github.com/globaltypesystem/gts-spec>
- **Constructor Fabric Foundation** — <https://www.constructorfabric.org>

---

## Executive Summary

If you build **multi-tenant XaaS / SaaS backends**, you are repeatedly solving the same problems in every service: tenant isolation, authentication/authorization, licensing & quota, usage metering, consistent REST APIs, pagination/filtering, observability, and safe DB access. Popular programming languages like Go and C# can solve these problems well, often with excellent libraries, analyzers, and mature team conventions.

Gears combines many of these best practices into one structured Rust middleware: shared platform contracts, reusable libraries ("gears"), consistent API patterns, and build-gated checks that make the preferred path explicit. Those checks help both human developers and AI-generated code follow established patterns, backed by compile-time guarantees and validation.

Gears takes a different position, in two layers:

1. **Rust as the language** moves some important classes of bugs — memory data races, use-after-free, null dereferences, many unhandled errors — from *runtime incidents* to *compile errors* or explicit types.

2. **Gears as the middleware** provides the platform layer around Rust: tenant-scoped data access, authentication/authorization contracts, consistent API construction, composable business capabilities, local-first testing, deployment options, supply-chain policy, and additional build-gated safety checks.

The result is a solid technology stack for long-living, comprehensive XaaS systems: product teams get a common foundation for security, tenancy, APIs, observability, lifecycle, and deployment instead of assembling those pieces differently in every service. This structure is especially useful for AI-driven development: coding agents work better when correctness rules are not only written in prose, but also expressed as types, generated schemas, lints, tests, and CI checks that provide deterministic feedback.

### At a glance: Go vs C# vs Rust vs Rust + Gears

For a long-living XaaS platform, Gears can provide advantages that are less visible in small standalone services: shared runtime capabilities, stronger static guarantees, consistent APIs, and platform rules enforced by the build.

A similar middleware could exist or be built for Go or C#, and mature teams often build parts of it successfully. However, this investment would still not close every Rust-specific advantage: ownership-based memory safety without a GC, `Send`/`Sync` data-race checks, exhaustive enums, zero-cost newtypes, and macro-generated code that remains type-checked by the compiler.

| # | Concern | Go (typical) | C# / .NET | Rust (plain) | **Rust + Gears** |
|---:|---|---|---|---|---|
| 1 | **Runtime footprint** | small binary, GC pauses | larger runtime, GC | small, no GC | small, no GC |
| 2 | **Deployment shapes** | per-service choices | per-service | per-service | **One code → edge / bare-metal / K8s** |
| 3 | **Memory & data-race safety** | GC; memory races possible, detected only at runtime (`-race`) | GC; memory races possible | Compile-time ownership & `Send`/`Sync` | Compile-time, same as Rust |
| 4 | **Error handling** | `if err != nil`, lint-gated in mature shops | exceptions / analyzer policy | `Result<T, E>`, `?` — must handle | `Result` + **canonical error taxonomy** (RFC-9457) |
| 5 | **Null safety** | `nil` panics | NRT helps, policy-dependent | `Option<T>` — no null | `Option<T>` everywhere |
| 6 | **Panics / unsafe shortcuts** | runtime panics possible | runtime exceptions possible | possible, but lintable | `unwrap`, `panic`, unsafe patterns [**prohibited at build time**](#b14-preconfigured-build-gated-safety) |
| 7 | **State evolution** | `switch` may miss new constants | `switch` often needs analyzer support | exhaustive `match` over enums | exhaustive `match` + architecture lints |
| 8 | **ID / domain mix-ups** | named types catch swaps, but literals/conversions remain easy; opaque structs add boilerplate | record structs / value objects work, but require conventions and serializers | zero-cost newtypes; no implicit conversion; private fields by default | tenant/user/resource IDs can be distinct validated types |
| 9 | **Scoped resource cleanup** | `defer` discipline | `using` / `IDisposable` discipline | `Drop` + lifetimes | scoped transactions, guards, spans, connections |
| 10 | **Polymorphism** | interfaces + newer generics; less expressive type relationships | rich generics + interfaces; runtime framework costs vary | traits, associated types, monomorphization | typed SDKs and `ClientHub` across transports |
| 11 | **Compile-time code generation** | generators / reflection / tags | source generators / attributes / reflection | procedural and declarative macros expand into type-checked Rust | derives and builders generate schemas, scopes, APIs, route metadata, and validation glue |
| 12 | **Tenant isolation** | manual `WHERE tenant_id = ?` | manual / EF global filters | manual | **Standardized** via `SecureConn` + `AccessScope` |
| 13 | **AuthN / AuthZ** | per-service middleware, bespoke | ASP.NET policies | bespoke | **Built-in** PDP/PEP (NIST SP 800-162) |
| 14 | **API consistency** | per-team router conventions | attributes + filters | bespoke | **`OperationBuilder`** → uniform REST + OpenAPI |
| 15 | **Pagination / filtering** | hand-rolled | OData libs | hand-rolled | **Built-in OData** `$filter`/`$select`/`$orderby` |
| 16 | **Architecture policy** | `go vet` / `golangci-lint` / custom checks | Roslyn analyzers / custom checks | Clippy / custom lints | Prewritten Clippy + architecture lints (via `cargo gears lint`) for Gears conventions |
| 17 | **Multi-tenancy / licensing / quota / usage** | build it yourself | build it yourself | build it yourself | **Pre-integrated, replaceable gears** |
| 18 | **Extensible API domain data types** | manual | manual | manual | **GTS** — versioned, schema-validated, autogenerated JSON schemas from Rust code |

Those benefits are not free. Rust and framework-heavy stacks have real adoption costs; Gears tries to make them visible, document where they matter, and reduce them with structure, tooling, templates, and automation rather than hiding them.

| # | Concern | Go (typical) | C# / .NET | Rust (plain) | **Rust + Gears** |
|---:|---|---|---|---|---|
| 1 | **Learning curve** | low; deliberately small language | moderate; familiar OO + large docs | high; ownership, lifetimes, async, traits | high plus framework concepts (*) |
| 2 | **Code readability for newcomers** | usually straightforward | usually familiar to enterprise teams | can be dense; types/macros/async add load | can be denser because framework types encode policy (**) |
| 3 | **Compile speed / iteration** | usually fast | usually good; tooling mature | often slower, especially large workspaces | slower again when full lint/test gates run |
| 4 | **Ecosystem breadth** | very strong for cloud/network services | very strong enterprise ecosystem | strong systems/backend ecosystem, thinner in some areas but evolving | inherits Rust tooling |

(*) Gears middleware consists of three large layers:
- **Toolkit** — reusable Rust libraries for API construction, data access, auth integration, observability, transport, testing, and other cross-cutting platform concerns.
- **System Gears** — platform-level capabilities such as events, tenant and authentication resolvers, type registries, serverless/runtime support, and other shared XaaS services.
- **Domain Gears** — product/business capabilities such as chat, credential storage, file parsing, approval flows, and other feature-level libraries.

Gears Toolkit internals can be complex because they encode reusable XaaS infrastructure. Individual Gear libraries are usually simpler: they model a concrete business capability inside established platform boundaries, with tenancy, API wiring, validation, and safety checks already provided by the stack.

(**) AI-assisted development changes the readability trade-off. Lints, generators, templates, examples, and deterministic validation help agents produce new Gear code faster and with fewer convention mistakes. Human reviewers still need to understand the code, but the generated result is usually ordinary business logic inside a familiar structure rather than a new service architecture invented from scratch.

> **TL;DR for Go devs:** You keep some operational properties Go teams often value —
> single binaries, fast startup, low runtime footprint, and explicit control flow.
> You do **not** keep Go's simplicity or ecosystem breadth. Gears is a trade:
> more language and framework complexity in exchange for stronger static checks
> and a pre-integrated XaaS backbone.

---

## Part A — Where Rust has advantages over Go and C# (for platform code)

This section is about the **language**. Gears is built on Rust specifically because it targets *long-lived platform code* where correctness and maintainability matter more than raw time-to-first-prototype.

### A.1 Errors are part of the type system

In Go, error handling is explicit and simple; serious teams usually add `errcheck` / `golangci-lint` to prevent accidentally discarded errors. Rust makes that stricter by putting fallibility in the type signature and making propagation (`?`) explicit in ordinary language flow.

```go
// Go — compiles unless your lint gate rejects the ignored error.
func loadUser(id string) *User {
    u, _ := db.FindUser(id) // ignored error; u may be nil
    return u
}

caller := loadUser("42")
fmt.Println(caller.Name) // nil pointer dereference at runtime
```

In Rust, the error is part of the type. You must deal with it, and there is no `nil`.

```rust
// Rust — you cannot accidentally ignore the error or deref a null.
fn load_user(id: &str) -> Result<User, RepoError> {
    let user = db.find_user(id)?; // `?` propagates the error explicitly
    Ok(user)
}

match load_user("42") {
    Ok(user) => println!("{}", user.name),
    Err(e)   => tracing::warn!(error = %e, "user not found"),
}
```

`Option<T>` replaces `nil`, and `Result<T, E>` makes "this can fail" visible in the signature. Whole categories of `nil` panics and swallowed errors become compile-time or lint-gated failures instead of review conventions.

### A.2 Memory data races are a compile error, not a `-race` flag

Go's race detector is excellent — but it only finds races on code paths you actually execute under instrumentation. Races ship to production all the time.

```go
// Go — compiles, runs, and corrupts the map under load. No compile error.
counts := map[string]int{}
for _, ev := range events {
    go func(e Event) {
        counts[e.Key]++ // concurrent map write -> runtime panic / corruption
    }(ev)
}
```

Rust's ownership model and the `Send`/`Sync` traits make unsynchronized shared mutable memory across threads a **compile error**. You're forced to use a proper synchronization primitive.

```rust
// Rust — won't compile unless the shared state is actually thread-safe.
use std::sync::{Arc, Mutex};

let counts = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
let mut handles = vec![];
for ev in events {
    let counts = Arc::clone(&counts);
    handles.push(std::thread::spawn(move || {
        *counts.lock().unwrap().entry(ev.key).or_insert(0) += 1;
    }));
}
```

"If it compiles, it's free of memory data races" is not a slogan — it's enforced by the borrow checker. This does not eliminate logical races such as TOCTOU bugs, lost updates, deadlocks, or bad transaction boundaries; those still need design, tests, and database constraints. For a platform handling concurrent multi-tenant traffic, Rust shifts an important class of concurrency defects from runtime testing into compilation.

### A.3 No `nil` interfaces, no exceptions-from-anywhere

C# gives you a rich runtime, but exceptions are invisible in signatures — any call can throw, and `NullReferenceException` remains the most common production failure. Go's `nil` interface trap (`err != nil` being true for a typed-nil) catches even experienced developers.

Rust has neither. Fallibility (`Result`) and absence (`Option`) are explicit in every signature, and exhaustive `match` means adding a new variant forces you to handle it everywhere.

**Gears** goes further than plain Rust style advice: nil-like failure paths and unsafe shortcuts are blocked by the build. Project policy treats `unwrap`, avoidable `panic`, unchecked assumptions, and unsafe patterns as architecture violations, not personal taste. In Go or C#, teams can enforce similar rules with analyzers, linters, and review policy; in Gears, Clippy plus project-specific lints make these checks part of the standard build gate.

### A.4 Sum types make illegal states unrepresentable

Modeling a state machine in Go usually means a struct with a bunch of optional fields and a comment explaining which combinations are "valid."

```go
// Go — nothing stops you from setting ErrorMsg while Status == "running".
type Job struct {
    Status   string // "pending" | "running" | "done" | "failed"  (by convention)
    Result   *Output
    ErrorMsg string
}
```

Rust enums carry data per-variant, so invalid combinations can't be constructed:

```rust
// Rust — the compiler guarantees a failed job has an error and no result.
enum Job {
    Pending,
    Running { started_at: Instant },
    Done { result: Output },
    Failed { error: String },
}
```

### A.5 Exhaustive `match` makes state evolution safer

In Go or C#, if you add a new status, old `switch` statements may keep compiling unless analyzers or strict review rules require every call site to be revisited:

```go
// Go — adding StatusCanceled later does not force every switch to be updated.
switch job.Status {
case StatusPending:
    queue(job)
case StatusRunning:
    observe(job)
case StatusDone:
    archive(job)
}
```

In Rust, matching an enum is exhaustive by default. If you later add `Cancelled`, every `match` that forgot it fails to compile until you decide what the new state means:

```rust
// Rust — adding Job::Cancelled forces this match to be updated.
match job {
    Job::Pending => queue(job),
    Job::Running { started_at } => observe(started_at),
    Job::Done { result } => archive(result),
    Job::Failed { error } => report(error),
}
```

The same pattern appears everywhere: `Option` forces you to handle absence, `Result` forces you to handle failure, and enums force you to handle new states. This is exactly the kind of language support you want when platform APIs and workflows evolve over years.

### A.6 Zero-cost abstractions and predictable performance

No garbage collector means no GC pauses and a small, predictable memory footprint — which is exactly what you want for edge/on-prem appliances and for running the *full* platform locally during development. You get C-like performance with high-level ergonomics, and the abstractions compile away.

### A.7 Tooling and static analysis as a first-class citizen

`cargo`, `rustfmt`, `clippy`, and `cargo-deny` give a consistent toolchain. Crucially, Rust's lint infrastructure is extensible — which is the hook Gears uses to enforce **architecture** at compile time (see Part B).

### A.8 Newtypes make identity mix-ups a compile error

Most backends have IDs like `UserId`, `TenantId`, `OrderId`. Even when they're all strings or UUIDs underneath, they should not be interchangeable — passing a `UserId` where a `TenantId` is expected is a bug the compiler should catch.

**Go — named types help, but are not a full validation boundary**

```go
type TenantID string
type UserID   string

func LoadTenant(id TenantID) {}

var u UserID = "user_123"
LoadTenant(u)              // compile error — good
LoadTenant("tenant_123")   // ALLOWED — untyped string literal converts implicitly
id := TenantID("anything") // ALLOWED — explicit conversion from any string
```

Even in this case two escape hatches remain: literals convert implicitly, and any caller can construct `TenantID(x)` from any string with no validation. To close them you have to fall back to a struct with an unexported field:

```go
type TenantID struct{ value string }

func NewTenantID(v string) (TenantID, error) {
    if v == "" { return TenantID{}, errors.New("empty tenant id") }
    return TenantID{value: v}, nil
}
```

This works, but there is a cost:

- **JSON / DB marshaling** must be hand-written (`MarshalJSON`, `Scan`, `Value`) per type, because the field is unexported.
- **Zero value is silently invalid** — `var t TenantID` produces `TenantID{""}` that bypassed the constructor.
- **Same-package code** can still write `TenantID{value: x}` directly, bypassing validation.
- The type is no longer interchangeable with `string` in generics, reflection, or struct tags.

**C# — similar story, with strong value-object options**

Plain primitives have the same swap problem as Go. A common fix is a `readonly record struct` or another value-object pattern:

```csharp
public readonly record struct TenantId(Guid Value);
public readonly record struct UserId(Guid Value);

void LoadTenant(TenantId id) { }

LoadTenant(new TenantId(userId.Value)); // explicit, distinct type
```

**Rust — the validated boundary is the default**

```rust
pub struct TenantId(String);
pub struct UserId(String);

fn load_tenant(id: TenantId) {}

load_tenant(user_id);              // compile error — distinct types
load_tenant(String::from("x"));    // compile error — no implicit conversion
load_tenant("tenant_123");         // compile error — &str is not TenantId
```

The inner field is **private by module** unless marked `pub`, so `TenantId(x)` is only constructible inside the defining module — no extra ceremony required. Validation lives in one place:

```rust
impl TenantId {
    pub fn new(v: String) -> Result<Self, IdError> {
        if v.is_empty() { return Err(IdError::Empty); }
        Ok(Self(v))
    }
}
```

Serialization, hashing, equality, ordering, and many schema/DB integrations can usually be derived or implemented once through standard traits instead of hand-written ad hoc codecs per call site. Newtypes compile to the same machine code as the inner type, so the type separation is free at runtime.

### A.9 RAII, lifetimes, and scoped resources

Rust also uses types to model resource lifetime. Values are dropped deterministically when they leave scope, so transactions, mutex guards, file handles, tracing spans, and pooled connections can release themselves through `Drop` even when a function returns early with `?`.

```rust
async fn update_document(repo: &Repo, id: DocumentId) -> Result<(), RepoError> {
    let tx = repo.begin().await?;
    let _span = tracing::info_span!("update_document", %id).entered();

    repo.lock_document(&tx, id).await?;
    repo.write_document(&tx, id).await?;

    tx.commit().await?;
    Ok(())
} // span exits; uncommitted guards/resources are dropped on every return path
```

Go's `defer` and C#'s `using` / `IDisposable` are good mechanisms, and experienced teams use them successfully. Rust's advantage is that scoped ownership is the default shape of the language. You can still design a bad transaction boundary or hold a lock too long, but it is harder to forget cleanup code in ordinary control flow.

### A.10 Traits and generics: zero-cost polymorphism

Rust traits are not just "interfaces". They support associated types, trait bounds, blanket implementations, static dispatch through monomorphized generics, and dynamic dispatch when you explicitly choose it. That combination is useful for framework code: APIs can be generic and strongly typed without requiring a runtime reflection model.

```rust
trait DocumentClient {
    type Error;

    async fn list_documents(
        &self,
        tenant: TenantId,
        owner: UserId,
    ) -> Result<Vec<Document>, Self::Error>;
}

async fn render_dashboard<C>(client: &C, tenant: TenantId, owner: UserId) -> Result<(), C::Error>
where
    C: DocumentClient,
{
    let docs = client.list_documents(tenant, owner).await?;
    /* ... */
    Ok(())
}
```

Go now has generics and remains excellent for simple interfaces. C# has a powerful generic system and mature runtime tooling. Rust's particular advantage for Gears is that SDK crates, in-process clients, and future out-of-process transports can share type-safe contracts while still compiling many abstractions down to direct calls. This is one reason the `ClientHub` model in B.7 can stay ergonomic without giving up type safety.

### A.11 Macros move framework rules into compile time

Rust macros are not only text substitution. Derive and procedural macros inspect Rust syntax at compile time and generate checked Rust code. That lets framework authors put repetitive correctness rules close to the type definition instead of relying only on runtime reflection, handwritten glue, or external code generation.

```rust
#[derive(Scopable, Deserialize, Serialize)]
#[secure(tenant_col = "tenant_id", owner_col = "owner_id")]
struct Document {
    tenant_id: TenantId,
    owner_id: UserId,
    title: String,
}
```

This is the bridge between plain Rust and Gears. `#[derive(Scopable)]` can generate scoping metadata from the entity type; `serde` derives serialization without handwritten mapping code; schema tools can derive OpenAPI or JSON Schema from Rust types; SQL tooling such as `sqlx::query!` can even check queries against a database schema at compile time when configured for that workflow.

Go and C# have strong alternatives: Go has explicit code generation and struct tags; C# has attributes, reflection, and source generators. Rust's advantage is the combination of type-checked generated code, no required runtime reflection, and close integration with the compiler pipeline. That is why B.5 and B.9 are not "magic": Gears leans on Rust's macro and lint ecosystem to turn framework conventions into code the compiler can verify.

---

## Part B — Why "just Rust" is not enough: what Gears adds

Rust gives you a safe language. It does **not** give you multi-tenancy, an authz model, a consistent API dialect, licensing, or a deployment story. In Go, C#, or Rust, teams still need to choose or build those platform conventions.

Gears is the **middleware and framework** that provides shared implementations and makes secure-by-default patterns the standard path.

### B.1 A pre-integrated configurable XaaS backbone

Multi-tenancy, permissions & roles, licensing & quota, usage collection, and an event system are all built in — and each is a **regular, replaceable gear** with its own SDK. You can swap Gears' `authn-resolver` / `tenant-resolver` for your existing vendor systems, or integrate an existing product catalog / license engine via plugins, **without modifying core gears**.

### B.2 Spec-driven development with Studio

Gears is designed to keep product and technical documentation close to the code and tests instead of treating it as a separate wiki that slowly diverges. The integrated Studio and spec-driven development workflow store architecture, requirements, decomposition, design, and feature documents alongside the implementation in markdown files.

Traceable IDs such as `cpt-*` connect specifications, generated code and tests. Deterministic checks validate document structure, templates, tables of contents, references, and traceability, so documentation defects can be found by tools rather than only by human review. AI workflows can then use those same IDs and validated artifacts for semantic spec-to-code transformation, gap analysis, and integrity checks after code changes.

For long-living XaaS systems, this matters as much as the runtime framework: requirements, APIs, tenancy rules, and operational decisions remain navigable and auditable as the system grows.

### B.3 Tenant isolation by default

One of the highest-risk bugs in a SaaS backend is a missing tenant filter — one missing `WHERE tenant_id = ?` can expose data across tenants.

In many Go or C# services, this is handled through query helpers, ORM conventions, middleware, or code review. Those approaches can work well, but they still depend on every code path using the right abstraction:

```go
// Go — one missing clause = cross-tenant data leak. The compiler is silent.
rows, _ := db.Query("SELECT * FROM documents WHERE owner = ?", userID)
// forgot AND tenant_id = ?  -> leaks every tenant's documents
```

In Gears, entities derive `Scopable`, and the recommended repository path uses `SecureConn` to apply the caller's `AccessScope` (tenant, resource, owner, type) as automatic `WHERE` clauses:

```rust
// Rust + Gears — scoping is applied by the framework from the SecurityContext.
#[derive(Scopable)]
#[secure(tenant_col = "tenant_id", owner_col = "owner_id")]
struct Document { /* ... */ }

// The AccessScope (derived from the authenticated caller) is applied automatically.
let docs = secure_conn
    .scoped::<Document>(&access_scope)
    .filter(documents::Column::Status.eq("active"))
    .all()
    .await?; // emitted SQL always includes the tenant/owner predicates
```

> The architecture makes the **scoped path the normal path**. Direct ORM or SQL access is
> reserved for infrastructure/migration code and guarded by review plus architecture lints.

### B.4 Authentication & authorization, built in (NIST SP 800-162 PDP/PEP)

Gears ships a real authorization architecture, not a middleware stub:

- **API Gateway** validates the token and injects a `SecurityContext`.
- The **PDP** (Policy Decision Point — an AuthZ Resolver plugin) evaluates policies
  (RBAC/ABAC/ReBAC — vendor's choice) and returns a decision **plus row-level
  constraints**.
- The **PEP** (Policy Enforcement Point — your domain gear) compiles those
  constraints into SQL `WHERE` clauses via `AccessScope`.
- Returns **predicates, not resource IDs** → one PDP decision per request, with the
  database applying row-level predicates for correct pagination and counts.
- **Fail-closed**: denied / unreachable PDP / missing constraints → `403`.

In Go or C#, a mature platform team can centralize this with shared middleware, repositories, analyzers, and code review. Gears' value is that this repo already provides a standard contract and implementation path for its services.

### B.5 Prewritten architecture lints (`cargo gears lint`)

This is where Gears uses Rust's linting model as a platform feature. This is not fundamentally different in kind from Go projects using `golangci-lint` / `go vet`, or .NET projects using Roslyn analyzers. The practical benefit is that Gears already ships a suite of custom architecture lints (run via `cargo gears lint`, provided by the `cargo-gears` CLI), and CI can fail the build on violations:

- **Domain-layer isolation** — no infra imports (`sqlx`, `sea_orm`, `axum`, `reqwest`) inside `domain/`.
- **Direct-SQL restriction** — raw SQL only in migration infrastructure.
- **Versioned REST paths** — endpoints must be `/<gear>/v1/...`.
- **Mandatory `OperationBuilder` metadata** — auth posture, error responses, and schemas must be declared.
- **GTS identifier correctness** — valid IDs; no `schema_for!` on GTS structs.
- **No unsafe shortcuts** — `unwrap`, avoidable `panic`, unsafe code paths, and unchecked invariants are treated as build-time failures where they would undermine platform guarantees.

Why this matters for Gears: the framework is not just a set of helper libraries; it is a **runtime contract** for secure XaaS systems. `cargo gears lint` lets the repository encode rules that ordinary Rust tooling cannot know: which layer may import which crate, which API paths must be versioned, which API metadata is mandatory, where SQL is allowed, and which GTS identifiers are valid. That turns some design-document rules into CI-enforced checks.

Compared with Go/C# alternatives, this is not about one ecosystem being incapable and another being capable. Go has `go vet`, `staticcheck`, and custom analyzers; C# has Roslyn analyzers; both are mature and useful. The difference is that Gears already includes project-specific checks for layer boundaries, route metadata, SQL placement, GTS identifiers, and unsafe shortcuts, wired into the same quality gate as formatting, Clippy, tests, and security checks.

> Documentation in markdown decays. `cargo gears lint` makes selected architecture rules executable in CI; that is not a substitute for design review, but it catches violations that reviewers would otherwise have to remember manually.

### B.6 Runtime Gears capabilities

Gears offer a set of useful capabilities for building modern XaaS systems:

- **Gear-owned migrations** — gears own their database migrations and run them as part of the runtime lifecycle, so schema ownership follows capability ownership.
- **Cluster primitives** — the cluster system gear provides common cross-instance coordination primitives: distributed cache, leader election, distributed locks, and service discovery. Operators can bind each primitive to the right backend for the deployment (in-process, Postgres, Redis, Kubernetes, NATS, etcd), while consumers keep the same facade-style API and get startup validation when a backend cannot satisfy required guarantees.
- **Transactional outbox** — reliable async message production with per-partition ordering, transactional or leased processing modes, retry/reject semantics, and graceful cancellation.
- **HTTP client** — `toolkit-http` provides a standard outbound HTTP client with rustls TLS, pooling, timeouts, retries with exponential backoff, User-Agent injection, fail-fast concurrency limiting, response size limits, transparent gzip/brotli/deflate decompression, and secure redirect handling with SSRF and credential-leakage protections.
- **SSE streaming** — toolkit support for typed server-sent events gives gears a standard way to expose streaming APIs without inventing one-off protocols.

### B.7 One consistent API dialect: `OperationBuilder` + OpenAPI + OData

In Go and C#, teams usually choose routers/frameworks and standardize conventions for auth, errors, pagination, and OpenAPI generation. Gears makes one such standard choice for this platform: a single authoritative route-registration mechanism where one declaration produces the route, the auth posture, the license posture, the schemas, the registered error responses, and the OpenAPI entry:

```rust
// Rust + Gears — one place declares everything; OpenAPI is generated from it.
OperationBuilder::get("/documents/v1/documents")
    .operation_id("documents.list")
    .summary("List documents")
    .authenticated()                       // auth posture is part of the route
    .require_license_features::<License>([])
    .handler(handlers::list_documents)
    .json_response_with_schema::<dto::DocumentPage>(openapi, StatusCode::OK, "OK")
    .error_401(openapi)
    .error_500(openapi)
    .register(router, openapi);
```

This gives gears one uniform place for pagination/filtering (**OData** `$filter`, `$select`, `$orderby`), auth, rate-limiting, timeouts, observability, and OpenAPI generation. It reduces drift, but still depends on routes using the standard builder and on CI/review catching bypasses.

### B.8 Composable gears: one codebase, many deployment shapes

A **Gear** is a self-contained unit that owns its API (an SDK crate), owns its data (behind `SecureConn`), is discovered at link time via `inventory`, and composes through a typed `ClientHub` in-process — or the *same* SDK over gRPC out-of-process.

The logical model is identical regardless of the physical boundary. Switching between in-process and out-of-process is a **YAML field** (`runtime.type`), not a code change:

- **Single-node** — all gears in one process → edge, on-prem appliances, dev/test.
- **Multi-node** — gears across processes/machines over gRPC, no orchestrator.
- **Kubernetes** — containerized, full orchestration, cloud-native ops.

> Develop locally single-node → deploy bare-metal → scale to K8s — **no rewrites**.

### B.9 Extensible domain model via the Global Type System (GTS)

Gears exposes extensible domain objects through [GTS](https://github.com/globaltypesystem/gts-spec): globally unique, human-readable, **versioned** identifiers (e.g. `gts.cf.core.events.event.v1~`) with JSON Schemas generated directly from Rust types and registered in a Types Registry. You can add new event types, settings, model attributes, permissions, or license types **without touching existing gears**. CRUD handlers are customizable via hooks/callbacks implemented as serverless functions or workflows.

### B.10 Canonical errors

Gears uses a canonical error taxonomy aligned with the 16 [gRPC error status codes](https://grpc.io/docs/guides/status-codes/) (`NotFound`, `AlreadyExists`, `PermissionDenied`, `InvalidArgument`, `Unauthenticated`, and others). Over HTTP, errors are rendered as **RFC-9457 `Problem`** documents, so REST handlers, SDK boundaries, and future gRPC transports share the same vocabulary. Handlers return typed domain errors; middleware maps them into stable wire responses with trace context.

### B.11 Observability and operational defaults

Gears standardizes operational concerns that are often re-created per service: OpenTelemetry tracing, request IDs, structured logs, health endpoints (`/health`, `/healthz`), timeouts, body limits, CORS/MIME controls, rate limiting, and inflight protection. This gives platform teams a common operational surface across all gears instead of a different observability story per service.

### B.12 FIPS 140-3 support

For regulated deployments, Gears can be built with `--features fips` to route TLS crypto through OS/provider-specific FIPS-capable modules such as Apple `corecrypto` on supported macOS configurations, AWS-LC FIPS on Linux, and Windows CNG on Windows. Validation status is provider-, platform-, and version-specific; see the security/FIPS docs for the supported matrix. This does not claim that Gears itself is a CMVP-listed module; it means Gears consumes validated cryptographic modules through a controlled TLS provider strategy.

### B.13 Supply-chain policy as code

Gears treats the dependency graph as part of the security boundary. The useful part is not the slogan; it is that dependency decisions live in files that reviewers can diff.

- **Same inputs for everyone** — `Cargo.lock` is committed, and `rust-toolchain.toml` pins the Rust version and components. A developer, CI job, and release build start from the same dependency closure and compiler baseline for a given commit.
- **License and advisory checks** — `deny.toml` defines the SPDX allowlist, RustSec advisory handling, allowed registries, and pinned per-crate exceptions. Exceptions include the crate, version, and rationale in the file, so a license or vulnerability exception is a code-review item, not a private spreadsheet.
- **FIPS graph checks** — `deny-fips.toml` is a stricter `cargo-deny` profile for `--features fips`. It blocks non-approved crypto crates and TLS backends outside the validated provider chain at build time. Phase A bans crates that should not enter the FIPS graph; Phase B records transitive cleanup before stricter bans such as `ring`, non-FIPS `aws-lc-rs`, pure-Rust AES/HMAC/HKDF/SHA paths, and other non-approved primitives can be enabled.
- **CI coverage** — CI runs `make deny`; OpenSSF Scorecard runs on a schedule; CodeQL Advanced scans Rust, Python, and GitHub Actions; ClusterFuzzLite runs PR-scoped Rust fuzz targets on PRs that touch code; and repository-controlled GitHub Actions are pinned by commit SHA rather than mutable tags.
- **Known gaps are explicit** — SBOM generation (CycloneDX), `cargo-vet` attestations, and SLSA provenance for release artifacts are useful next steps, but they should be treated as roadmap items until wired into CI.

This is not unique to Rust — Go, C#, and other ecosystems can and should enforce dependency policy too. Rust's advantage for Gears is that the build graph, lockfile, feature flags, advisory database, and `cargo-deny` checks compose naturally with the same automation model used for formatting, lints, tests, architecture rules, and FIPS-mode builds.

### B.14 Preconfigured build-gated safety

Gears also defines a workspace lint floor in `Cargo.toml` and `clippy.toml`. This answers the practical question "what does prohibited mean?" with concrete build rules, not style guidance.

- **Unsafe and debug leakage** — `unsafe_code = "forbid"` applies workspace-wide. `unwrap_used`, `expect_used`, `dbg_macro`, `use_debug`, and `unnecessary_debug_formatting` are denied; tests are the explicit exception for `unwrap`/`expect` through `clippy.toml`.
- **Async correctness** — `await_holding_lock`, `await_holding_refcell_ref`, `async_yields_async`, and `unused_async` are denied. These catch common Tokio mistakes: holding a lock across `.await`, carrying `RefCell` borrows across suspension points, and adding unnecessary async boundaries.
- **Numeric safety** — `cast_possible_truncation`, `cast_possible_wrap`, `cast_precision_loss`, `cast_sign_loss`, `integer_division`, `float_cmp`, and `lossy_float_literal` are denied. For quotas, usage metering, billing, and limits, silent narrowing or precision loss must be explicit and reviewed.
- **Complexity budget** — `cognitive_complexity`, `type_complexity`, `too_many_lines`, and `struct_excessive_bools` are denied with project thresholds: cognitive complexity `20`, type complexity `190`, function length `200`, and at most `2` bool fields in a struct. The bool limit nudges state modeling toward enums instead of ambiguous flag bags.
- **AI-generated code guardrails** — the config includes stricter thresholds for LLM-generated code: `single-char-binding-names-threshold = 4`, `large-error-threshold = 128`, plus denials for redundant clones, needless collects, verbose patterns, large stack arrays, `Rc<Mutex<_>>`, and `LinkedList`. This catches the kind of plausible-but-bloated code that agents and humans both produce under time pressure.
- **Tenant-safe ORM use** — `clippy.toml` configures `disallowed-methods` for direct SeaORM `all`, `one`, `count`, update, and delete execution methods, with reasons pointing developers to secure scoped wrappers.

Go and C# teams can enforce many of these rules with `golangci-lint`, `go vet`, Roslyn analyzers, and custom build policy. The practical difference in Gears is that the Rust compiler, Clippy, custom architecture lints (via `cargo gears lint`), and Cargo feature checks are already wired into one standard workspace safety pipeline (`make clippy`, `make lint`, `make safety`).

### B.15 Local-first, shift-left development

Because gears are composable libraries, the **full business logic — including scenarios that span multiple gears** — can be run and tested locally on a developer machine, without Jenkins, Ansible, or K8s. A single process can host many gears and exercise cross-gear flows end-to-end entirely in memory.

Gears comes with integrated unit, integration, end-to-end, and fuzzing tests, plus coverage and diff-coverage to show exactly which changes are exercised. The same test suites can then be repeated by CI against a distributed deployment, where real networking, orchestration, and database backends are also exercised. This **local-first, fully testable runtime** lets developers and Agentic IDEs/LLMs catch logical and cross-gear issues early, *before* a pull request is opened, so most behavioral defects are found locally long before CI or a release stage.

---

## When Gears is (and isn't) the right choice

**Choose Gears when you are:**

- A **XaaS / SaaS vendor** building on a governed, multi-tenant backbone.
- A **platform/product team** that wants security and tenancy handled by shared platform defaults instead of one-off service conventions.
- A **GenAI builder** needing chat, RAG, model management, agents, tools.
- An **on-prem / edge vendor** shipping single-binary appliances.
- An **enterprise** embedding capabilities into an existing platform via plugins.

**Gears is deliberately *not*:**

- Optimized for minimalism / the absolute lowest barrier to entry — it prioritizes explicit structure, security, governance, and evolvability.
- A ready-to-use catalog of end-user services — it's the *foundation* vendors build on.
- A replacement for cloud infrastructure or PaaS — gears are libraries and building
  blocks.

---

## Get started

```bash
git clone --recurse-submodules https://github.com/constructorfabric/gears-rust
cd gears-rust

make build      # build libs + example server
make example    # run the example server -> http://127.0.0.1:8087/cf/docs

curl http://127.0.0.1:8087/cf/health    # detailed JSON (all component checks)
curl http://127.0.0.1:8087/cf/readyz    # readiness: 200 ready / 503 not ready
curl http://127.0.0.1:8087/cf/healthz   # liveness "ok"
```

**Next steps**

- Read the [Architecture Manifest](./ARCHITECTURE_MANIFEST.md) for the full rationale behind the Rust and monorepo choices.
- Browse the [Gears inventory](./GEARS.md) to see what's already built.
- Follow the [Toolkit guide](./toolkit_unified_system/README.md) to build your first
  gear.

---

*Constructor Fabric Gears (Rust) · Apache-2.0 · by the
[Constructor Fabric Foundation](https://www.constructorfabric.org).
Secure · Modular · Composable · GenAI-ready.*
