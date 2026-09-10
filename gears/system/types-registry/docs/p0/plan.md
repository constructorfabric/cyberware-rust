# Implementation Plan: Types Registry P0

Spec: [`SPEC.md`](./SPEC.md)
Task list: [`todo.md`](./todo.md)

> **Location note.** These three artifacts live with the gear they describe, in
> `gears/system/types-registry/docs/p0/`, not in a repository-root `tasks/`. This is
> deliberate: the monorepo holds many gears, and a shared root `tasks/` would collide
> across concurrent work. Downstream commands that default to `tasks/todo.md` — including
> `/agent-skills:build` — must be pointed at `gears/system/types-registry/docs/p0/todo.md`.

## Overview

Make Types Registry durable: entities move from a process-local `gts-rust` store into the
platform database, admission becomes an asynchronous operation-based protocol, effective
artifacts are materialized, registration moves from a registry-side inventory pull to a
per-gear push, and a new SDK trait replaces the old one outright.

Global entities only — no tenant ownership, no `PlatformSecurityContext`, no PDP, no
federation.

**Coordination state, stated once for the whole plan.** `types_registry__coordination_state`
exists — it is created and seeded by this P0's second migration. Only its
`entity_write_order` row is seeded and used in P0: the serialization point every commit
that writes entity state advances as its first statement (P15 below). `source_claim` is
not created in P0. The `routing` state row — the future routing generation — arrives with
federation, seeded by that phase's own migration alongside `source_claim`. A standalone
`routing_config` table is never created, in P0 or any later phase.

`limits.resolved_document` and `limits.resolution_closure` are enforced during admission
and dependent refresh. Both must be positive. Closure accounting is per document over the
candidate overlay; the resolved-size budget applies to the canonical bytes of each effective
artifact. Exceeding either refuses the candidate without committing partial state.

32 tasks in 8 phases with 8 review checkpoints — 30 planned up front, plus T9a and T24a added
out of the Checkpoint 1 review (P12/P13). Twenty-nine are S or M; three are **L** and say
why in their own entry — T25 and T26 (consumer migration across twenty-plus gears) and T28
(e2e migration across seven files), each split by gear or by file rather than landing as one
commit. Two tasks exceed the ~5 file guideline, flagged with the reason where they occur.

## Decisions taken during planning

Sixteen decisions were made here rather than in the spec, because all of them are consequences
of task ordering or of facts about the runtime that only surface once the work is sliced.
P1–P5 were taken before implementation started; P6–P10 came out of reviewing Phase 1 on its way
in, and the spec has been updated to match all five. P12 is a correction: it reverses a change T9
made to the existing v1 REST contract, and adds T9a and T24a. P13 is a reordering — Instances
into Phase 1, and `make dylint` per phase instead of per task. P14 defines `x-gts-ref`
independently of the dependency graph. P15 replaces the revision-vector lock with the optimistic
guard and keeps the one thing that survives the argument: a serialized write path — one row,
claimed first by every commit — that orders commits, joined by every writer of entity state:
admission here, deletion at T20, purge under ADR-0013. P16 and P17 came out
of reviewing Phase 3 on its way out: P16 makes observability a per-task obligation from T17
onward, and P17 moves T27 — the REST surface — from Phase 7 into Phase 5. (P11 was a
housekeeping close-out and is retired; the number is not reused.)

### P1. The spec's §15 build order is replaced by vertical slices

§15 orders work horizontally — schema, then repositories, then "synchronous admission
core", then operations. Its slice 3 explicitly builds a synchronous admission path that
slice 6 then converts to the asynchronous one. That is the same work twice and throws away
slice 3's tests, so it is not followed.

Instead each phase from 1 onward delivers **one complete registration path**, async-shaped
from the first commit. Phase 1 registers a single dependency-free global Type Schema
end to end: migration → entity → repository → transient store → acceptance → worker → REST.
Later phases widen that path without reshaping it.

No decision from SPEC §3 changes. Only the order does.

### P2. Types-registry seeds itself by invoking the admission worker directly

types-registry owns the `toolkit-gts` base types and its own control-plane types (DESIGN:
*"`toolkit-gts` base types default to `types-registry` ownership"*). It cannot register
those through a client — it *is* the registry, same process, same database.

It resolves itself from §8.1: the worker is a plain function of `(operation_id, runner)`,
not a task. So `init()` creates the operations and calls the worker **inline**, with no
outbox and no barrier. This is not a privileged second path; it is the same function the
outbox handler calls. Seeding is deterministic and complete before the client is published.

This is also what makes the §13 no-polling rule satisfiable, so it is the same property
being exploited twice.

### P3. The outbox worker starts at the end of types-registry's `init()`

Verified phase order (`libs/toolkit/src/runtime/host_runtime.rs:6-15`):

```
pre_init (system) → DB migrations → init (all gears) → proxy-wiring
→ post_init (system) → REST wiring → gRPC → start/stop (stateful)
```

`init` of **every** gear precedes `start` of **any**. A worker living in the stateful
`start` entry would therefore not exist while consumers are initializing — and ~13 existing
call sites already register from their own `init()` and block on the result. Those would
hang.

That is avoidable. `GearCtx` exposes `cancellation_token()`
(`libs/toolkit/src/context.rs:166`) and `OutboxHandle::stop()` is an ordinary async
shutdown (`libs/toolkit-db/src/outbox/manager.rs:620`), so the worker starts from `init()`
with correct cancellation wiring and stops from the stateful `stop`. `init` order is
topological and types-registry is a declared dependency of its consumers, so its worker is
live before any consumer's `init()` body runs.

**Order inside types-registry's `init()`:** repositories → inline seeding of
its own types (P2) → start the outbox worker → publish the client. Seeding precedes the
worker start and enqueues nothing, so seed operations cannot be leased concurrently. There is
no snapshot-load step: per P6 seeding builds its own transient store like any other
admission, and reads go to the database.

The resulting rule has no phase caveat: **after types-registry's `init()`, submit and await
work from anywhere.** A consumer registering during its own `init()` must declare
`deps = [types_registry]`, or `init` ordering is not guaranteed.

**Who admits, and when.** Acceptance and admission are separate moments with separate
executors:

| Moment | Accepts | Admits |
|---|---|---|
| types-registry `init()` — its own types | types-registry | itself, inline, no outbox (P2) |
| A gear reconciles its inventory | registry code in the **caller's** task (local client in-process; a gRPC client out-of-process) | the outbox worker |
| REST at runtime | types-registry's Axum handler | the outbox worker |

Acceptance is always synchronous, in the caller's task. Admission is performed by exactly
one outbox worker owned by types-registry — one in the system for a single-binary
deployment.

### P4. Registration moves from pull to push, in P0

A gear does not know whether it runs in-process or out of process, and its code must not
depend on that. The registry-side **pull** violates this in the worst way: a gear's code is
unchanged, but its types silently vanish if it moves out of process, because
`all_inventory_type_schemas()` only sees `inventory` records linked into *this* binary.

The platform already runs the transparent mechanism for half of this. Roughly eleven plugin
gears already **push** their well-known Instances by calling `registry.register(...)` from
their own `init()` through the ClientHub-resolved SDK trait — in-process that is the local
client, out of process it would be a gRPC client, and the gear's code is identical either
way. Only inventory-declared *schemas* still travel by pull.

P0 therefore moves schemas onto the same path, as DESIGN already specifies: *"The SDK
filters records by `owning_gear` and reconciles them, replacing the current registry-side
process-wide pull with a per-gear push that works across processes."*

- types-registry seeds **only what it owns** — `toolkit-gts` base types and its own
  control-plane types — inline, per P2.
- Every other gear reconciles its own declarations with **one SDK call**. The five-step
  reconciliation workflow of DESIGN §3.3 lives in the SDK, not in each gear, so no gear
  hand-rolls batching, idempotency or retry.
- `InventoryTypeSchema` / `InventoryInstance` gain `owning_gear`, derived from the declaring
  crate's gear name, so the SDK can filter and attribution stops being a constant.
- **`cfg.entities` is outside P4's scope.** It carries operator-controlled identities whose
  GTS identifiers are deployment-specific and cannot be expressed as gear-owned inventory items
  (e.g. the platform-root tenant type). These are seeded into the database at startup through
  the same inline admission path as types-registry's own inventory (T24). They are not
  reconciled through the SDK — no gear owns them; the deployment operator does.

**This is where registrant-side retry becomes real** — and it lives in the SDK helper. The
earlier answer that no retry was needed was conditional on keeping pull.

It also simplifies the cutover. Seeding no longer has to topologically order ~200 entities
across every gear at startup; types-registry seeds its own small set, and cross-gear
ordering is handled by the retry DESIGN sanctions: *"dependencies converge through retry."*

Cost: a `toolkit-gts` and macro change, plus one line in roughly fifteen gears. Benefit:
ceiling C3 disappears, out-of-process operation is unblocked, and the transparency
requirement actually holds.

### P5. The old SDK trait is removed in P0, not deprecated alongside the new one

Taken here first; SPEC **D6 now records it**, so the two agree. The version of D6 this
replaced kept both traits and deferred consumer migration to a separate commit.

Two facts forced the change. First, async admission makes the old synchronous
`register(Vec<Value>) -> Vec<RegisterResult>` unrepresentable: thirteen call sites call
`RegisterResult::ensure_all_ok(&results)` immediately and would start reading `pending` as
success. Keeping the old trait means keeping a blocking submit-then-await adapter behind a
signature that no longer describes what happens. Second, the old models cannot cross a wire
at all — `GtsTypeSchema.parent: Option<Arc<GtsTypeSchema>>` and
`GtsInstance.type_schema: Arc<GtsTypeSchema>` are in-process object graphs — so retaining
them retains an out-of-process blocker that P4 exists to remove.

So the old trait goes, and every consumer migrates inside P0. The real surface is larger
than the thirteen register sites: reads dominate (`list_instances` ~59 references,
`get_type_schema_by_uuid` ~31, `get_type_schemas_by_uuid` ~28), for roughly fifty files
across twenty-plus gears. Migration is split by gear group across two tasks so no single
task carries it all.

### P6. No store is held between admissions: transient store, reads from the database

This replaces the original SPEC D2 and §8.2, both of which have been rewritten. It changes
T5 and T8; nothing else in the graph moves.

The original shape was one immutable `ArcSwap` snapshot of the `gts-rust` store, loaded from
the whole entity table at init and rebuilt after every successful admission unit, serving
both semantic evaluation and consumer reads. Reviewing T5 before building it surfaced that
this conflates two needs with different answers.

**What actually needs a `GtsStore`** is semantic computation over related documents —
resolution, `compare_documents`, derivation chains, instance validation. All of it happens
inside admission, over one candidate and what that candidate consumes. That set is exactly
the dependency closure, which the `dependency` table already supplies (D5).

**What reads need is rows.** Verified rather than assumed:
`InMemoryGtsRepository::list` (`in_memory_repo.rs:241-274`) iterates the store as a plain row
container and filters with `GtsIdPattern`, which is a pure function in `gts-id` over the
identifier string — it never asks the store a semantic question. Exact reads are keyed
lookups. And D3 already materializes `resolved_schema` / `effective_traits` /
`effective_traits_schema` on the current-state row. So a read is a `SELECT` plus, for a
pattern query, `GtsId::matches_pattern` in Rust. Nothing about GTS semantics is
reimplemented, so `constraint-gts-implementation` is untouched.

**And the snapshot was not merely unnecessary for reads, it was wrong for them.** SPEC §13
requires *"two pods, commit on A, B's first post-commit read sees it"*
(`nfr-multi-pod-correctness`). P0 has no cross-pod invalidation — no pub/sub, and the outbox
belongs to the committing pod — so B would serve its stale snapshot indefinitely. Admission
survives stale input because of the commit-time revision-vector guard (D4, T15); reads have
no guard, so for them staleness is simply incorrect. This would have shipped as a passing
implementation of a failing criterion.

Consequences, including the cost:

- Reads become a database round trip, which is what the SDK client cache is for. **The
  earlier decision to delete that cache is reversed** — see P7.
- Ceilings **C1** (whole entity set in memory) and **C4** (startup reads the whole table) are
  retired rather than deferred: there is no warm-up read and no process-lifetime store.
- `Mutex<GtsOps>` disappears instead of being replaced. A store owned by one worker
  invocation is never shared, so `GtsOps` not being `Sync` stops being a design constraint.
- The transient store is *cheaper* than what it replaces: the snapshot cost a full rebuild
  after every successful unit; the closure is bounded by the candidate's own dependencies.

### P7. The SDK client cache is kept, not deleted — DESIGN requires it

This reverses a removal both earlier revisions of the spec carried, and it adds **T30**.
SPEC §8.3 is new and records the contract.

DESIGN requires a client cache outright: `cpt-cf-types-registry-fr-client-cache`, *"bounded
per-client representation cache with batched conditional revalidation and fail-closed expiry
handling"*, with the full contract in DESIGN §3.3. The removal rested on two claims, and
neither holds.

The first was ours and expired with P6: *"an LRU in front of an in-memory snapshot is pure
overhead plus a staleness window."* True while reads came from memory; once reads are a
database round trip, a cache buys what it costs.

The second was a misreading, and worth naming precisely because it nearly shipped.
`nfr-cache-correctness` forbids *"an invalidated result accepted as current after the client
observes the mutation"* — it does not forbid a freshness window, and DESIGN §3.3 says so in
as many words: *"a remote mutation not yet observed may produce a stale snapshot within the
bounded window but is not described as an invalidated entry accepted as current."* The window
is the sanctioned trade. And it is a different NFR from `nfr-multi-pod-correctness`, which
governs the **registry's** reads — *"no process-local authority"* — and which P6 satisfies.
Two NFRs about two different sides of the wire were treated as one.

So P0 builds DESIGN's cache minus what needs absent inputs: bounded store, freshness window,
`fresh` bypass, invalidation on an observed terminal outcome, dual identifier/UUID indexing,
and DESIGN's list of what is never cached. Batched conditional revalidation against freshness
validators was first deferred here with tenancy, but P9 moves it into P0 once the validator
inputs are shown to be platform-plane computable. Deferred with tenancy, then, are only the
projection / visibility / Context-Tenant key dimensions. Recorded as ceiling **C7** rather
than left implicit.

Two things this changes beyond the decision:

- **The bound becomes bytes.** Today's default is `capacity: 1024` entries while §3.2 caps
  one resolved document at 1 MB, so the configured bound permits ~1 GB. DESIGN makes the
  same argument and picks 64 MB; adopted. This is a live bug in the current defaults, not a
  new requirement.
- **The cache cannot be carried over as-is.** It is typed on `GtsTypeSchema` / `GtsInstance`,
  which P5 deletes, so it is ported onto `EntitySnapshot`.

**Ordering.** T30 lands last, after the cutover rather than with it, and that is deliberate:
the cache is an optimization over a read path that must be correct first, and T24 is already
the largest task in the plan. The cost is one window — T24 through T28 — where reads are
uncached. Checkpoint 7 gates on T30 being done, so P0 does not finish without it.

### P8. P0 ships the platform-plane API on the business listener; the plane is contract-deep, not transport-deep

SPEC §8.4 is rewritten and ceiling **C8** is added. No task moves; T9 and T27 gain criteria.

Registering a global entity is a platform-level operation, and P0 already treats it as one in
the data — §8.1 writes `plane = 1`, `tenant_id = NULL` on every operation record. The earlier
§8.4 nonetheless marked "Tenant REST" as the P0 surface and deferred everything platform,
which left the spec claiming a tenant-plane transport for platform-plane rows. So: **the P0
REST surface and SDK are the platform-plane API for global entities** — async registration and
reads, no tenant ownership — and that is what the e2e suites exercise.

The plane is not enforced by the transport, because the platform offers an in-process gear no
way to do it. Verified in the code, not assumed:

- `internal_auth_middleware` — inbound platform plane over HTTP — is installed only in
  `libs/toolkit/src/runtime/oop_serve.rs:390`, the per-gear server for a gear running *out of*
  process. api-gateway's `internal_auth` is an outgoing gRPC credential for DirectoryService
  (`gears/system/api-gateway/src/gear.rs:823-826`), not an inbound validator.
- api-gateway has one API listener; its only second listener is for health probes. ADR-0006/0008
  ask for a separate platform listener, which nothing implements.
- `OperationBuilder` has no `.platform()`, and the middleware is permissive — a missing token
  passes with no `PlatformSecurityContext` (`toolkit-http-middleware/src/auth.rs:220`).

**The routes therefore keep the authentication they have.** Switching them to `.anonymous()`
to signal "not tenant traffic" would be a security regression, not progress: with no platform
identity available it would let anything reaching the gateway register a global type. Same
shape as the PDP deviation — authenticated, not authorized, gap named (C6, now C8).

One trap avoided: advice to serve platform routes `.anonymous()` and **not** `.exposed()` is
written for a gear's own OoP listener. `exposed` defaults to `false` (internal-only), so
copying it here would make the routes unreachable for the e2e suites that must call them.

**The later gRPC move is expected, not contingent.** A REST contract method taking
`&PlatformSecurityContext` first is rejected at compile time —  *"generated client cannot
source the internal token… serve over gRPC or write a manual client"*
(`toolkit-contract-macros/src/rest_contract_parse.rs:337-347`, UI test
`rest_platform_secctx_rejected.rs`). P0's client is REST-generatable precisely because it
carries no platform identity; adding identity closes REST codegen and leaves gRPC or a manual
client over `attach_internal_token_http`, which today has zero call sites in the repository.
Recorded in §8.4 so the P1 decision is a consequence someone already wrote down.

### P9. Freshness validators are in P0 — the deferral rested on a misread input table

SPEC §8.5 is new, ceiling C7 shrinks, and **T29** is added; T23 and T30 gain criteria.

Both earlier revisions listed *"freshness validators, `ETag` / `If-None-Match`, conditional
reads"* as out of P0 because they *"need the validator inputs tenancy supplies."* That reason
does not survive DESIGN §3.3's own input table
(`cpt-cf-types-registry-tech-freshness-validator`):

| Validator input | Managed | In P0 |
|---|---|---|
| `entity.resource_version` | ✓ | yes — CAS is already P0 (T11) |
| `type_schema.resolution_fingerprint` | ✓, Type Schemas only | yes — materialized by D3 (T8) |
| subject visibility-chain version | ✓ **tenant plane only** | **not applicable** — DESIGN: *"a platform read has no subject visibility chain"*, and every P0 read is platform-plane (P8) |
| Context Tenant availability-chain version | ✓, only when availability is selected | not applicable — availability is out of P0 |
| routing generation | — external only | not applicable — federation is out of P0 |
| `external_revision`, `content_hash` | — external only | not applicable — Externally Managed Entities are out of P0 |
| normalized projection | ✓ | yes — no `$select` in P0, and DESIGN says absent `$select` *equals an explicit default set*, so it is a constant marker |

The tenant inputs are not missing in P0; they **do not participate** in a platform-plane read.
So a P0 validator is fully computable: a versioned digest over `resource_version`,
`resolution_fingerprint` and a projection marker. DESIGN even fixes the wire form — base64url
of a versioned JSON object, 128-bit managed digest, ~48 characters.

The framework is not a blocker either. `OperationBuilder::no_content_response` takes an
arbitrary status, so `304` is declarable, and `file-storage` already returns
`StatusCode::NOT_MODIFIED` with headers by hand
(`gears/file-storage/file-storage/src/api/rest/handlers.rs:212`). There is no ETag helper in
the toolkit — this is manual work with a working precedent, not missing capability.

**What made this urgent rather than merely available.** The validator has to be in the SDK
models from **T23**. Adding it afterwards is a breaking change to a contract that ~50 call
sites across twenty-plus gears will already have moved onto (T25, T26). Deferring validators
would therefore not have been a neutral scope cut — it would have bought a second migration.

Consequences:

- Ceiling **C7** shrinks from *"the cache expires rather than revalidates"* to just the
  missing key dimensions: T30's cache now does DESIGN's batched conditional revalidation and
  fail-closed expiry, which is what `fr-client-cache` actually requires.
- A `304` replaces a resolved document of up to 1 MB on the hot read path, which is the
  cheapest thing available for `nfr-lookup-latency` after D3.
- The digest must carry the projection marker from day one; otherwise a P1 `$select` token
  produces a false `unchanged` (RFC 9110 §8.8.3). The versioned wire form is the escape hatch,
  but paying one field now is cheaper than relying on it.

**Ordering.** T27 (the routes, in Phase 5 per P17) → T23 (the field in the models) → **T29**
(computation, `ETag`, `304`, per-key batch validators) → T30 (cache revalidates against them).
T30 was renumbered from T29 to keep task numbers in dependency order; T27's number is the one
documented exception (P17).

### P10. Discovery is paged and content-free in P0; `$select` and expansion stay out

SPEC gains decision **D12** and a rewritten §10.2; §2's row is split. T4, T23, T27 and T28
gain criteria; no task is added.

`Discovery cursors, $select projections, OData pagination, expand_type_filter` were listed out
of P0 with the reason *"P0 keeps the current flat list"* — a restatement of the decision, not a
reason for it. Examined item by item, the four are not one decision:

**Pagination and cursors are in.** Three facts settled it. SPEC §10.1's own trait already
returns `EntityPage`, so deferring the cursor left a page that is a page in name only — the
spec contradicted itself. DESIGN specifies the route as *"`200` with one page and a cursor"*
over *"content-free discovery"*. And the cursor's inputs degenerate exactly as the validator's
did: of the seven DESIGN binds into it — query, subject visibility context, Context Tenant,
authorization scope, routing generation, per-source position, running item count — P0 keeps
**two**, query and position, because the rest are tenant-plane, PDP, or federation. Position is
free: T27 already required ordering by canonical identifier, so the cursor is a keyset over a
unique immutable column, and `toolkit-odata` (`page.rs`, `pagination.rs`) already encodes
cursors as versioned base64url that refuse an unknown version.

**The current shape is also a live problem, not only a spec gap.** `GET /entities` returns
every match in one array with each item's full `content`; with artifacts materialized (D3)
that is *entity count* × up to 1 MB, and after the pull→push cutover the count is every gear's
declarations. A `limit` alone would not have fixed it — without a cursor the bound makes the
endpoint incomplete rather than large, which is why D12 lands both together.

**The default projection is in; arbitrary `$select` is out.** The default field set is what
makes a page content-free, so it is not optional. Caller-chosen sets need optional fields
across the models plus a normalized field-set digest inside the validator, and buy nothing
while there is a single representation to select from — that half stays in ceiling C7.

**`expand_type_filter` is genuinely blocked**, and this is the one item whose original
placement was right for the wrong reason. Its DESIGN definition *is*
`$select=gts_uuid&availability=available`, with the availability filter fixed by the method
rather than supplied by the caller. Availability (ADR-0010) needs tenancy and is out of P0, so
a P0 method under that name would report retired contracts as usable. A same-named different
meaning is worse than absence; a caller wanting the traversal pages `list_entities` itself.

**The consequence to plan for, because it lands in consumer code.** `list_instances` and
`list_type_schemas` are helpers over `list_entities`, and their call sites read payloads from
the result. Against a content-free page the helpers hydrate through `batchGet`: one extra round
trip per page, absorbed by the client cache on repeat, complete with respect to the traversal
rather than to an instant. That is the same trade DESIGN accepts for expansion, but it means
T25/T26 migrate call sites onto a two-step read rather than a renamed one-step read.

### P12. v1 stays intact; the async surface ships as v2 and is promoted at T24a

T9 repointed the two existing v1 routes — `POST /entities` and `GET /entities/{gts_id}` — at the
database path instead of adding new ones. That contradicts the invariant in the risk table below
(*"The DB path has no consumer until T24; no dual-write"*), and it costs more than a red e2e
suite:

* the `POST` body shape changed and gained a required `Idempotency-Key`, so existing callers
  get `400`/`422` — not the status-code break T28 was scoped for;
* `testing/e2e/gears/oagw/helpers.py` and `testing/e2e/gears/account_management/conftest.py`
  register over REST and then resolve through `TypesRegistryClient`, so both gears write to the
  database and read from process memory. That is a functional cross-gear regression, and no e2e
  edit repairs it — only T24 does;
* `GET /entities` (list, memory) and `GET /entities/{entity_key}` (database) gave one resource two
  sources of truth.

So the surface becomes **additive**: v1 is restored verbatim from `main` and keeps serving the
in-memory store, while T9's async surface moves to `/types-registry/v2/` (T9a). The two stores
stay unreconciled — no dual-write, no fallback read — which is P6 enforced rather than merely
intended: with a fallback, an admission that never happened would read as success.

**v2 is interim, and its retirement is planned rather than assumed.** T24 deletes the in-memory
repository, so the v1 routes reading it are deleted in that same task, and **T24a** promotes v2
onto the v1 paths. Recommended Phase 7 order was **T24 → T27 → T24a → T28**: T27 authors the
remaining routes once, on v2, and the rename happens after them; T25/T26 float, since they depend
on T24 alone. **P17 supersedes that half:** T27 moved into Phase 5, so the routes exist before the
cutover and Phase 7 is **T24 → T24a → T28**, with T24a promoting all seven at once. Nothing else
in this decision changes — v1 and v2 stay separate stores until T24, and the promotion is still
where the break reaches a v1 caller.

**What the T24–T26 window owes `TypesRegistryClient`.** T25 and T26
migrate consumers off the *Rust trait*, not off `/v2/`, so T24a's rename costs them nothing and
their placement after it is right. The gap is one task earlier: T24 deletes the in-memory
repository the old trait's implementation reads, while ~13 `register(...)` sites and every read
site stay on that trait until T25/T26. Its `register` is synchronous, and after T24 the only store
is the asynchronous one. **The decision, rather than a note that one is needed:** the old trait keeps its shape over the
window and its `register` becomes a **submit-then-poll shim** over the one database store, deleted
by T26 with the trait. It is not a dual path in P6's sense — one store, one write path, no
fallback read — and it is what keeps the workspace building while ~15 gears migrate one at a time.
The alternative, folding T25 and T26 into T24, is rejected: T25 is ten gears and their plugins and
T26 five more, which is not work that shares a task with the cutover, and a task that cannot
compile until all of it lands is not a task. T24 carries the criterion; *"repointing it at the
database would be a compatibility shim with no consumer"* (T24a) stays true of the **routes** and
was never true of the trait.

**What this buys, concretely.** `make e2e-local` stays green from here to T24 with no e2e file
edited, and the red window shrinks from ~19 tasks to the T24–T28 stretch, where the wire break is
real and unavoidable. What it does *not* buy is an earlier cutover: the SDK and every consumer
stay on the in-memory store until T24, which is P6's design and not a gap. The earliest honest
cutover needs Instances (T10), revisions (T11), reference and derivation edges (T13) and
dependency-aware batching (T19) — without them the first `register` from `oagw` or
`account-management` fails, since both push batches of derived schemas and instances.

### P13. Instances move into Phase 1; `make dylint` runs per phase, not per task

Two ordering changes, taken after Checkpoint 1's report.

**T10 moves from Phase 2 into Phase 1.** Instances are not a widening of the path — they are
what the platform pushes today. P4 counts *"roughly eleven plugin gears"* already registering
their well-known Instances from their own `init()`, so Instance support is on the critical path
to T24 and is the longest pole in it. T9's surface also accepts an Instance and then fails it in
the **worker** (`StoreBuildError::UnsupportedKind` → `WorkerError::StoreBuild` → opaque `500`, a
retryable class for a final decision); building the feature closes that hole rather than adding a
refusal for it, so no separate task is needed.

The move drags in three companions, listed in T10's own entry. The one worth naming here is the
**identifier-derived closure**, because it corrects a claim Phase 1 committed to code:
`DependencyRepo::closure` walks the `dependency` table only, so nothing until T13 could reach a
candidate's base — and `admission_worker_test.rs` asserts a derived Type Schema *fails*, blaming
T13's missing edges. That is half right. `GtsId::chain_ids()` and `get_type_id()` are pure
functions of the identifier, so a derivation base — and an Instance's conforming type — need no
edge table at all. Seeding the closure worklist with the chain as well as the edges is what makes
T10 cheap, and it admits derived Type Schemas in Phase 1 as a side effect. T13 supplies the
`$ref` targets needed by the forward closure and the direct edge set needed by T14's reverse
walk. `x-gts-ref` is neither resolved nor represented by an edge.

Phase 1 therefore delivers one global entity **of each kind**, and Phase 2 becomes revisions and
concurrency. T12's *kind* rule moves with T10 — a Type Schema `…ns.thing.v1~` and an Instance
`…ns.thing.v1` derive the same `family_key` — while shape and contiguity stay in T12.

**`make dylint` moves from per-task verification to the phase checkpoint.** It builds the whole
workspace, so per task it is the most expensive check on the list and the one that gets skipped;
per phase it is cheap enough to actually run. The exposure is bounded — phases 2 through 7 are two
to four tasks each.

**The per-task standing bar had to move with it.** `todo.md` required *"`make ci` green"* of every
task, and `make ci` ends in `dylint` — so the old bar cancelled this decision on
the line above it, which is why T1–T9 each recorded `make ci` as *partial*. The bar is now
`make fmt`, `make clippy` and gear tests per task; the full `make ci` — `dylint`, `deny`,
`lychee`, `gts-docs` and four container targets — is a checkpoint gate. Bare `make ci` lines are
gone from individual tasks, and where one carried something specific (`lychee` on the two
documentation tasks) that check stayed and only `ci` went.

**And `make test-db` was never the right command for this gear.** It runs `cf-gears-toolkit-db`'s
own suite and never builds `cf-gears-types-registry` — a task could satisfy that line without
executing one line of the gear, which T2, T4, T5, T7 and T8 each noticed separately. The two
container suites now have a target of their own, `make test-types-registry-db`,
in `make ci` beside `test-users-info-pg` and `test-usage-collector-pg`. `todo.md`'s Commands
section is the single definition; the tasks say *gear tests* and point at it.

The counter-evidence is real, and is why this is a decision rather than a convenience: the first
run happened at Checkpoint 1 with 26 violations standing across T1–T9, in three families
(DE0708, DE1302, DE0301). A phase is a far shorter accumulation window than nine tasks, and
layering violations are cheap to fix in bulk because they are mechanical.

**The per-task records go with the requirement.** T1–T9 each carried a `make dylint` line
recording that it had not run; with no per-task requirement there is nothing for those lines to
record, so they are removed rather than left standing as unmet criteria. Nothing observed is lost:
the workspace-wide run at Checkpoint 1 covers every one of those tasks, and it is where the
findings are recorded. Checkpoint 0's gate is ticked from that same run — Phase 0 is one task and
the run included its changes. Phase 1's run covers T1–T9 only, so Checkpoint 1 carries an explicit
**re-run** item for T9a and T10.

### P14. `x-gts-ref` is not a dependency edge

`x-gts-ref` constrains an instance value to match a GTS identifier pattern. It does not resolve
or inline the entity that the value names and therefore creates no dependency edge.

* **Validation.** `gts-rust` enforces the keyword by matching the value string against the
  pattern — `XGtsRefValidator::validate_value_matches_gts_pattern` parses the value, parses the
  pattern, compares. It never consults the store. So the constraint is satisfiable with nothing
  registered under the pattern.
* **Artifact refresh (T14).** An `x-gts-ref` target is not inlined — DESIGN §3.1 excludes it from
  the resolution closure by name. Revising the target therefore cannot change the constraining
  schema's artifacts. Including it in a reverse walk would spend
  `limits.activation_write_set` on branches whose effective artifact cannot change.
* **Deletion safety (T20).** The platform provides no referential-integrity guarantee for the
  keyword: an `x-gts-ref` naming `topic.v1~` will **not** block deleting `topic.v1~`. A value
  naming a deleted entity stays structurally valid, the registry sees no runtime data that would
  make the refusal meaningful. Instance values are likewise not scanned for identifiers they
  contain.
* **Admission policy.** The managed–external boundary classifies entity-naming patterns directly
  from candidate content when federation is introduced. Major-0 quarantine has no such check:
  changing a named v0 entity cannot change the constraining schema's accepted payloads.

The stored edge kinds are `1 schema_ref, 2 derivation, 3 instance_of`, and
`ck_tr_dependency_kind` admits exactly those values. The numbering is append-only after the
first release.

**Renumbering them in the initial migration is safe here, and this is why.** `main` carries
`1 schema_ref, 2 gts_ref, 3 derivation, 4 instance_of` and the same initial migration, so a
deployment on `main` with a database has applied that migration and will not re-apply the
edited one. Nothing is mis-decoded regardless, because **no supported production path in
`main` can have persisted a `dependency` row**: `DependencyRepo::replace_outgoing` has no
caller there outside tests — `main`'s own T13 entry says so — and this branch is where
admission first calls it. So no row exists whose `kind` could be reinterpreted, and the only
residue on an
already-migrated database is a laxer `CHECK` (`IN (1,2,3,4)` where the edited migration writes
`IN (1,2,3)`), which admits a superset of what the code can now produce. Once a release has
persisted an edge, this argument expires and the numbering is append-only, full stop.


### P15. Locking the revision vector is the wrong tool; serializing commits is right

SPEC §8.1 step 4.2 asked for two lock levels: the version family, then *"candidate and
revision-vector entity/current rows in canonical identifier order"*. T15 implements the first and
not the second — and the second is not a shortfall to be made up later. It is the wrong mechanism,
and DESIGN §4 has been corrected rather than deviated from.

**A lock cannot do the guard's job.** It guarantees only that nothing moves *after* it is taken,
and the movement that matters happens between evaluation and the lock — the phantom dependent
appears before any lock could be held. So the vector comparison is required whether or not rows
are locked, and the lock is purely additive: what it buys is that a contended admission waits
instead of rolling back. Liveness, not correctness.

**And it is expensive in exactly the place this design pays attention to.** One round trip per
vector member, inside the commit transaction, on a set `activation_write_set` allows to reach 512
— the cost T14 restructured the reverse read into a single CTE to avoid. It is also the only
reason step 4.2's canonical ordering needs to extend past families: order matters because the
locks are many. Remove them and the requirement disappears with them. Optimism is the shape of
the rest of this design anyway — `resource_version` compare-and-swap, a transient store per unit,
validation outside any transaction — and registrations are rare against reads, which is the
regime optimistic detection is for. That the secure API has no `FOR UPDATE` and `SQLite` has no
row locking is corroboration, not the argument: the argument stands if `FOR UPDATE` arrives
tomorrow.

**What holds instead.** The candidate's own row is serialized by the compare-and-swap that writes
it. A dependency that moves is serialized by the refresh its mover owes the dependants: that
refresh writes each affected dependant's `type_schema` row, the same row this commit writes, so
the two block on one another — which orders them and nothing more, since a refresh computes its
artifacts before it writes. What makes the loser notice is that the refresh's write is a
compare-and-swap on the revision and fingerprint it read; it rolls back and recomputes. Where the
change leaves a dependant's fingerprint unmoved, nothing is written and nothing was stale. SPEC §8.1
step 4.2 records the argument, the one window it leaves, and the liveness cost by name.

**One lock survives the argument, and it orders commits.** The argument covers everything that
meets on a row. What it cannot cover is an edge committed *after* a mover's reverse scan: adding an
edge moves no `resource_version` and writes only `dependency`, so the two commits write no row in
common, both pass their own guards, and the dependant keeps an artifact inlined from a revision
that is no longer current with a fingerprint that matches it. The requirement is therefore a **serialized write
path**: every commit claims the `entity_write_order` row of `types_registry__coordination_state`
as its transaction's first
statement, one commit at a time per installation. It works because the reverse-impact scan and
the vector guard already run inside the commit transaction: either the edge is visible to the
mover's scan, or the unit writing it has not committed and its own guard catches the mover. Two
cases, no third. A row rather than an advisory lock, because advisory keys live on a session
separate from the transaction's connection and losing it would release the key while the
transaction carried on. This is nothing like a lock over the vector, which stays the optimistic
guard for the window between evaluation and the claim. Every writer of entity state claims it:
admission here, deletion at **T20**, the purge job under ADR-0013. DESIGN §3.7 states it.

**The `unchanged` outcome is not guarded, deliberately.** Step 4.3 sits ahead of every write, and
an `unchanged` candidate performs none: no revision, no version move, no refresh. The one thing it
decides — that the authored content already equals the current revision — is decided from rows
read inside its own transaction, so no part of it rests on the evaluation's view. Guarding it
would take a genuine no-op re-submission, make it revalidate because a *neighbour* moved, and
after `worker.max_revalidation_attempts` such moves turn it into a failure. So the guard runs
after that branch, and `an_unchanged_resubmission_is_not_refused_by_a_moved_dependency` is what
would catch the mistake.

**One consequence worth naming:** `limits.activation_write_set` is now asked twice, because the
vector's reverse-impact read is the same read the refresh does. An over-bound candidate is
therefore refused at evaluation, before any transaction has written, under the same
`activation_write_set_exceeded` reason — strictly earlier and cheaper, and invisible to a client.
T14's refusal stays as the backstop for a set that grew in between. Both ask the same question,
because D5 states the bound over the set the walk *returns* rather than over the rows the
fingerprint filter ends up writing — the written set is a subset, and only the walked set is a
number either read has before it writes.

### P16. Observability is a per-task obligation from T17 on, not a second T16

SPEC **§8.6 is new** and records the contract this decision enforces; success criterion **16** is
added, so P0 does not finish with an undiagnosable decision on the write path.

T16 instrumented the admission path *as it stood at the end of Phase 3*. Every decision Phase 4
and Phase 5 add — a compatibility verdict, a forced waiver, a quarantine refusal, a deletion, a
dry run — is one T16's instruments either cannot see or cannot separate from something else.
Checked in the code rather than assumed:

* `AdmissionMetrics` (`domain/ports/metrics.rs`) has five methods and **no `kind` and no
  `dry_run` parameter**. So a deletion's success and a registration's success are one series, and
  a dry run that wrote nothing would increment `candidates_total{status="succeeded"}` beside the
  commits that did. Both spans already carry `kind` and `dry_run`, so the gap is in the metrics
  only — which is why this decision is about labels and not about spans.
* Acceptance-stage refusals are enumerable because `AcceptanceError::reason()` is an exhaustive
  match, and T16's claim that *"a refusal a later task adds cannot compile until it has a
  reason"* is true **of acceptance**. Admission-stage reasons are `ItemFailure::new("literal",
  …)` at ten-odd call sites, and nothing makes a new one appear in any vocabulary. `Unknown`,
  `blocked_by_*`, the quarantine refusals and deletion's dependent check would each be countable
  only if someone remembered.
* `Unknown` is the one verdict SPEC §16.12 requires to be distinguishable, and the only one a
  deployment has reason to alert on. Counted as one `reason` among a dozen it loses exactly what
  makes it special: it is a fail-closed refusal, not a candidate decided against.

**So each task instruments what it adds, in its own commit**, and there is no follow-up
observability task to defer. The rule, stated once here and carried as criteria in T17, T18, T19
and T20:

1. **Every new terminal outcome and every new refusal is countable under a closed vocabulary**,
   with no identifier ever a label. `refusals_total{stage,reason}` carries the refusals; a new
   instrument appears only where a label on an existing one would misreport — which is the case
   for the compatibility verdict, because `compatible` is not a refusal and has nowhere else to
   go.
2. **A series that blends writes with non-writes is wrong.** `dry_run` becomes a label wherever
   a series would otherwise mix a rollback-only pass with a commit, and `kind` wherever it would
   mix a deletion with a registration. T20 does that sweep in one commit, across every instrument
   that exists by then, because it is the task that makes both distinctions real.
3. **The admission reason vocabulary has one home, and it is compile-enforced.**
   `ItemFailure::new` takes `AdmissionFailureReason`, defined in `domain::admission::reasons`.
   Each task adds its refusal variants there. Stored and API codes remain strings;
   `ItemFailure::from_payload` restores known variants and preserves unfamiliar codes as
   `Unknown(String)`. Known reasons keep their metric labels after reading from storage;
   unknown codes map to the single `other` label.
4. **The evidence bar is T16's**, because that is what makes a dashboard contract real: rendered
   names, label keys and label *values* asserted against an `InMemoryMetricExporter`; the
   emission asserted end to end through the real `accept` / `run_operation`; and a mutation check
   that stripping the emission fails the tests.

**One correction to T16's record, while it is being extended.** `todo.md`'s T16 entry and its
commit message both argue for a process-global instrument set reached like `tracing`. The code
that shipped does not do that: the instruments are behind
`domain::ports::metrics::AdmissionMetrics`, the OpenTelemetry adapter is in `infra::metrics`, the
handle is injected from `init()` and carried down the call graph as an `Arc`, and the name prefix
is configurable. The port is the better shape — `de0301_no_infra_in_domain` cannot see an
infrastructure type that hides at the crate root — and it is the shape to extend, so the record
is corrected rather than the code. Only `observability.rs`'s two span constructors are free
functions, and its module header states why.

### P17. T27 moves into Phase 5, behind T20 — the REST surface is not cutover-dependent

T27's declared dependencies were T21 and T23. Neither survives inspection.

* **T21 is not a dependency.** Admission already runs inline — `AdmissionMode::Inline` with
  `NullDispatch` (`gear.rs`), written that way at T7 precisely so the outbox could arrive later —
  so a `POST` to `/v2/` admits end to end today and every route T27 adds is exercisable without
  an outbox. T21 changes *who calls the worker*, not what a route can do.
* **T23 is a contract dependency, and the contract is already fixed** by SPEC §10.1 and §10.2:
  `items` as the array name on all three batch bodies, `key` as the per-item entity name,
  `EntityPage` as the page shape. T27 authors DTOs against that section and T23 authors the trait
  against the same one. Where the two could still disagree they disagree while both sit behind
  `/v2/` with no consumer, which is the cheapest place in the plan for that to happen.

What T27 *does* depend on is **T20**: `:batchDelete` and `DELETE /entities/{entity_key}` are one
route pair over the deletion path, and by T27's own criteria they add no logic the domain does not
already expose. So the task moves whole to the end of Phase 5, and the phase becomes batching,
deletion, dry run **and the REST surface that exposes them**. `GET /entities` — the paged,
content-free discovery route of D12 — therefore lands two phases earlier than planned, which is
the point: it is the route with the live problem (every match in one array, each with up to 1 MB
of content), and it is the one every consumer and every e2e suite reads through.

Three consequences, each tighter than what it replaces:

* **P12's ordering constraint disappears.** The recommended Phase 7 order was
  T24 → T27 → T24a → T28, with the promotion wedged between the last new route and the e2e
  migration. With the routes authored in Phase 5, Phase 7 is **T24 → T24a → T28**, with
  T25 → T26 running alongside it — T25 on T24, T26 on T25, since T26 deletes the trait T25's
  gears are still on. T24a promotes all seven routes at once, because all seven already exist.
* **The changelog entries stay in Phase 7, with T24a.** T27 carried them, but the break they
  describe happens at the promotion — a changelog entry announcing a v1 break in a release where
  v1 still works is simply wrong. T24a already required both entries as *"one release, two
  entries"*; it now owns them outright.
* **T27's `make e2e-local` verification is replaced, not moved.** P12's invariant is that no e2e
  file is edited before T24, and T27 now lands five tasks ahead of it. The new routes are
  verified through the real router in `tests/api_rest_test.rs` and by manual `curl` against
  `/v2/`, while `make e2e-local` stays green **and untouched** — it becomes a criterion of T27
  rather than its verification. T28 remains the single task that migrates the Python suites, and
  it still runs against the promoted v1 paths.

**The number stays T27**, out of dependency order, which this plan otherwise avoids — T30 was
renumbered from T29 for exactly that reason. Renaming costs two dozen cross-references across two
documents to satisfy a convention whose only reader is the plan itself, so the exception is
documented here instead. **T29's dependency on T27 is satisfied two phases early**; its remaining
one is T23, and it stays in Phase 7 beside T30, which needs T24 and T26 regardless.

## Dependency graph

```
T1 gts-rust 0.12.0  ─────────────────────────────────┐  (blocks all: semantics change)
                                                     ▼
T2 migration ──► T3 entities ──► T4 repositories ──► T5 transient store ──┐
                                        │                                 │
T6 config ──────────────────────────────┴──► T7 acceptance ──► T8 worker (single candidate)
                                                                   │
                                                        T9 REST: POST, GET op, GET entity
                                                                   │
                              T9a v1 restored; async surface on /v2/ (P12)
                                                                   │
                              T10 instances + chain-derived closure; family kind (P13)
                                                                   │
                              ─── Checkpoint 1 ───
                                                                   │
                                                        T11 revisions + CAS
                                                                   │
                                          T12 family shape + contiguity
                                             │
                              T13 dependency edges (3 kinds; only $ref is content-derived)
                                             │
                     ┌───────────────────────┼───────────────────────┐
                     ▼                       ▼                       ▼
        T14 reverse impact      T15 revision-vector guard    T16 observability
                     │                       │
                     └───────────┬───────────┘
                                 ▼
                     T17 compatibility ──► T18 derivation + quarantine
                                 │
                     T19 partial admission ──► T20 delete + dry run
                                                        │
                     T27 REST completion + OpenAPI + QUICKSTART (P17)
                                                        │
        ┌────────────────────────┬──────────────────────┴─┐
        ▼                        ▼                        ▼
   T21 outbox        T22 toolkit-gts owning_gear    (T19 enables T24)
        │                        │
        └──────────┬─────────────┘
                   ▼
        T23 new SDK trait + reconciliation helper
                   │
        T24 CUTOVER: registry seeds only its own; ready mode + in-memory repo out
                   │
        T24a retire v1; promote v2 → v1 — all seven routes at once (P12, P17)
                   │
        ┌──────────┴──────────┐
        ▼                     ▼
   T25 migrate system    T26 migrate domain gears,
   gears + plugins       delete the old trait
        └──────────┬──────────┘
                   ▼
        T28 e2e suites move to submit-then-poll

        T29 validators + conditional reads (needs T23; T27 landed in Phase 5)
                   │
        T30 SDK client cache on EntitySnapshot — revalidates against T29 (P7)
```

Foundation order (T2→T5) is unavoidably layered: nothing can be registered before a table
exists. From T7 onward the graph is vertical.

## Task index

### Phase 0 — Upgrade (fail fast)
- T1: Upgrade to `gts-rust` 0.12.0, re-validate all declared identifiers

**Checkpoint 0**

### Phase 1 — One global entity of each kind, persisted, async, end to end (fixtures only)
- T2: Migration for the 9 tables
- T3: SeaORM entities for the core six
- T4: Repositories on `DBRunner`
- T5: Transient `gts-rust` store built from database rows
- T6: Typed configuration
- T7: Acceptance path and operation records
- T8: Admission worker — one dependency-free candidate
- T9: REST — `POST /entities`, `GET /operations/{id}`, `GET /entities/{entity_key}`
- T9a: Restore the v1 contract; the async surface moves to `/types-registry/v2/` (P12)
- T10: Registered Instances — **moved here from Phase 2** (P13)

**Checkpoint 1** ← proves the architecture

### Phase 2 — Revisions and concurrency
- T11: Content revisions and compare-and-swap
- T12: Version-family kind, shape and contiguity rules

**Checkpoint 2**

### Phase 3 — Dependencies and materialization
- T13: Dependency edge extraction and writes
- T14: Reverse-impact worklist and artifact refresh
- T15: Revision-vector guard and bounded retry (P15)
- T16: Observability for the admission path

**Checkpoint 3**

### Phase 4 — Compatibility
- T17: Compatibility against one baseline — verdicts counted, `Unknown` and `force` visible (P16)
- T18: Derivation chain and major-0 quarantine — each refusal its own counted reason (P16)

**Checkpoint 4**

### Phase 5 — Batching, deletion, dry run, and the REST surface
- T19: Dependency-aware partial admission
- T20: Deletion and Dry Run — plus the `dry_run` / `kind` label sweep (P16)
- T27: REST completion, OpenAPI, QUICKSTART — **moved here from Phase 7** (P17)

**Checkpoint 5**

### Phase 6 — Dispatch and the new contract
- T21: Outbox dispatch wiring
- T22: `toolkit-gts` — `owning_gear` on inventory records
- T23: New SDK trait and the reconciliation helper

**Checkpoint 6**

### Phase 7 — Cutover and migration
- T24: **Cutover** — registry seeds only what it owns; ready mode and in-memory repository out
- T24a: Retire v1; promote v2 → v1 (P12) — lands right after T24, promoting all seven routes (P17)
- T25: Migrate system gears and plugins onto the new trait
- T26: Migrate domain gears; delete the old trait
- T28: Update e2e suites for the `202` contract
- T29: Freshness validators and conditional reads (`ETag` / `304`, batch validators)
- T30: SDK client cache — window, byte bound, `fresh`, conditional revalidation

**Checkpoint 7 — ready for review**

## Checkpoints

Each checkpoint is a human review gate. Do not proceed past a failing one. **Every checkpoint
runs `make dylint` over the full workspace** — per phase rather than per task (P13); Checkpoint 0
is left as recorded, with T1's documented exception.

**Checkpoint 0** — `make ci` green; every declared GTS identifier still admits under
0.12.0; every difference in generated schema documents accounted for. This gate protects
other gears, so it is reviewed before any registry code is written.

**Checkpoint 1** — a fixture Type Schema registers over REST, the operation reaches
`completed`, the entity and its resolved artifacts are readable, and both survive a process
restart. **The new surface is additive (T9a, P12): v1 is intact, `make e2e-local` is green and no
e2e file was edited.** An Instance registers against a Type Schema committed by an earlier
operation, and a derived Type Schema admits against a committed base with the `dependency` table
empty (T10, P13). Consumers are untouched: the old trait is still served from its existing in-memory
repository, while the new path reads from the database and holds no store between admissions
(P6). The plain gear tests are green on SQLite,
`make test-types-registry-db` is green on PostgreSQL and MySQL, and `make dylint` is re-run
after T9a and T10 — the recorded run covers T1–T9 only (P13). This checkpoint proves the
architecture.

**Checkpoint 2** — equal content reports `unchanged` without a revision;
a stale `expected_resource_version` fails `precondition_failed`; family shape and contiguity
refusals hold under concurrency.

**Checkpoint 3** — a revision of a base type refreshes every dependent's artifacts in one
transaction; an identical recomputation moves no `resource_version`; the activation bound
refuses rather than partially committing; admission emits spans and metrics.

**Checkpoint 4** — the compatibility matrix passes, including `Unknown` rejected with its
own reason; provenance is persisted on every revision. **Every verdict is counted and `Unknown`
and a forced waiver are each distinguishable in the metrics, and admission reasons live in one
compile-enforced vocabulary** (P16) — quarantine and dialect refusals included, none of them
collapsed into `invalid_schema`.

**Checkpoint 5** — a batch with a failing dependency commits independent branches and
blocks everything downstream of it; a circular `$ref` is refused; Dry Run writes nothing.
**No series blends a dry run with a commit or a deletion with a registration, and blocked
candidates are counted per reason** (P16). **The REST surface is complete on `/v2/`** (T27, P17):
all seven routes are in OpenAPI, `GET /entities` returns one bounded content-free page whose
cursor traverses the set exactly once and refuses `$select`, and `make e2e-local` is still green
with no e2e file edited.

**Checkpoint 6** — an operation submitted through the outbox reaches `completed` without a
direct worker call; inventory records carry `owning_gear`; the new trait and its
reconciliation helper work against a mock consumer. Nothing has been cut over yet.

**Checkpoint 7** — every gear reconciles its own declarations and gates its own readiness;
the platform boots; the old trait is gone and no consumer references it. The SDK client cache
is in place on the new models, with its window, byte bound and `fresh` bypass (P7) — P0 does
not finish with an uncached read path. **One REST version: no `/v2/` path survives, and the
in-memory repository and its routes are gone (T24a, P12).** Discovery and the batch routes still
behave as Checkpoint 5 proved them, now on the promoted v1 paths. All 16 success criteria of SPEC §16;
`make ci`, `make test-types-registry-db`, `make e2e-local`, `make dylint` green.

## Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| 0.12.0 semantics reject a currently-admitted schema in another gear | **High** — breaks unrelated gears | T1 is first and is its own commit; full re-validation sweep before any registry code |
| Pull→push cutover regresses platform boot | **High** — every gear now gates on its own registration | T24 lands after the helper is proven against a mock (Checkpoint 6); migration split across T25/T26 by gear group; each verified by booting the example server |
| Removing the old trait breaks ~50 call sites in 20+ gears | **High** | Split by gear group; new trait exists and is tested (T23) before the first consumer moves; `cargo test --workspace` gates each migration task |
| A gear's registration fails at startup and it gates readiness on it | Medium | This is DESIGN-intended (*"Each gear gates only its own readiness"*), but it is a real behavioural change; the SDK helper retries, and failures name the gear and identifier |
| Dual path (in-memory + DB) live through phases 1–5 | Medium | The DB path has no consumer until T24; no dual-write, no reconciliation between them. P6 keeps them from converging by accident: the new path holds no persistent store, so there is no second copy of entity state that could drift from the old repository. **This was breached by T9 and repaired by T9a (P12):** repointing the v1 routes made the DB path consumer-visible ~19 tasks early, and `oagw` / `account-management` were then registering into the database while resolving from memory. The mitigation is now structural — v1 and v2 are separate routes over separate stores, and the criterion "no route straddles the two stores" is grep-checkable |
| DB revisions land before reverse-impact refresh and compatibility | Medium | T11 documents the staging window; minor-bearing Type Schema revisions and effective `force` are refused, and the DB path has no consumer until T24. Checkpoints 3 and 4 must close T14/T17 before cutover |
| Read latency regresses at T24, when reads move from memory to the database | Medium | Correctness first, then the cache: D3 already materializes what a read returns, so a read is one keyed `SELECT`, and T30 restores caching with DESIGN's contract (P7). The exposure is the T24–T28 window, which is why Checkpoint 7 gates on T30 |
| A cached entry can be stale inside its freshness window | Low | DESIGN §3.3's sanctioned trade, and now bounded further: T29's validators let T30 revalidate rather than guess, `fresh` gives an authoritative read, `0s` disables the window, and invalidation is immediate on an observed terminal outcome |
| The validator field reaches the SDK models after consumers have migrated | **High** — a second migration across 20+ gears | T23 carries the field from the start, before T25/T26 move any consumer; T29 only fills it in (P9) |
| A materialized `effective_*` value differs from the deleted client-side computation | Medium — reads as a regression, invites a "fix" back to the old wrong answer | 12 call sites in `account-management`, `resource-group`, `credstore` consume those methods today. The old ones resolved only the parent `$ref` and approximated trait defaults (`TODO(#1723)`), so `gts-rust` is authoritative; T25/T26 carry an explicit criterion to accept the new value, and SPEC §13 pins the outside-the-chain `$ref` case as a test |
| Content-free discovery turns one-step list reads into list + `batchGet` at ~87 call sites | Medium | The SDK helpers hydrate internally, so call shapes survive (P10); the client cache absorbs the second trip; T23 fixes the helper shape before T25/T26 touch a consumer |
| `GET /entities` shape change reaches e2e alongside the `POST` break | Medium | Both are the same migration in T28, behind the one shared helper it already owns; the route's declared stability is `unstable`. Under P12 both arrive at the same moment by construction: T24 deletes old v1, T24a promotes the whole async surface at once |
| Concurrency protocol wrong under the least-tested backend (MySQL) | Medium | Plain gear tests on SQLite plus `make test-types-registry-db` on PostgreSQL/MySQL at every checkpoint |
| The `POST /entities` 202 break reaches other gears' e2e suites | Medium | Confirmed surface: 6 types-registry e2e files (~95 references to `/entities`) plus `account_management/conftest.py` and — **missed until P12** — `oagw/helpers.py`, which registers a batch of schemas *and* instances and reads them back through the list route. T28 owns the migration behind one shared polling helper, not open-coded loops. The break itself no longer arrives at T9: T9a keeps v1 intact, so the suite goes red at T24 and green at T28 rather than being red for ~19 tasks |
| T27's v2 DTOs are authored before T23 fixes the SDK trait shape | Low | The contract is SPEC §10.1/§10.2, not either task: `items`, `key`, `EntityPage`. Both are written against that section, and a disagreement surfaces at T23 while the routes are still behind `/v2/` with no consumer (P17) |
| The new routes sit on `/v2/` for two extra phases without e2e coverage | Low | They were never e2e-covered before T28 either — P12 forbids editing an e2e file before T24. Coverage is `tests/api_rest_test.rs` through the real router plus manual `curl`; T28's scope is unchanged, and T27 now carries "`make e2e-local` still green, no e2e file edited" as a criterion (P17) |
| A later refusal or outcome ships without a metric, silently emptying a panel | Medium | P16 makes it a compile error rather than a review item: `ItemFailure::new` takes a `Reason` newtype whose only constructors are the vocabulary's consts, `dry_run` and `kind` become required port parameters at T20, and each of T17–T20 carries T16's evidence bar — contract test, emission test, mutation check |
| Activation write set exceeds the measured 27 in a future deployment | Low | Configured bound 512, refuses rather than partially commits (T14) |

## Parallelization

- **Parallel:** T16 with T14/T15. T22 with T21. T25 and T26 split per gear, but T26 deletes the
  shared trait and so lands after T25 — the split is within each, not between them. T30 with T28 — it needs the new models (T26) and the database read path (T24), and
  nothing in the e2e task touches the client cache.
- **Sequential:** T2→T5 (foundation), T7→T8, T13→T14→T15, T19→T20→T27 in Phase 5 (P17 — the
  deletion routes are one pair over T20's path), T22→T23→T24, and in Phase 7 T24→T24a→T28: the
  promotion now sits directly after the cutover, because every route it promotes already exists.
- **Contract first, then parallel:** T23's trait shape is fixed by SPEC §10.1, so it can
  start as soon as Checkpoint 4 passes.
