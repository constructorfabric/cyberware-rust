# SPEC — Types Registry P0

Derived from [DESIGN.md](../DESIGN.md), [PRD.md](../PRD.md), [database.sql](../database.sql).
P0 is a scope cut of Product P1, not a separate design: every decision below either
implements a DESIGN clause or records an explicit, named deviation from one.

Status: **approved**. The live execution artifacts are [`plan.md`](./plan.md) and
[`todo.md`](./todo.md); where this document and the plan disagree on *ordering*, the plan
wins (it supersedes §15 — see `plan.md` P1). Where they disagree on a *decision*, that is a
bug in this document: report it rather than picking one.

---

## 1. Objective

Turn Types Registry from an in-memory registry into a durable one, and replace its
SDK with a contract that survives the move.

Three deliverables:

1. **Persistence** — admitted entities live in the platform database, survive restart,
   and are visible to every pod.
2. **Registration API (Rust SDK + REST) for global entities** — no tenant ownership.
   The current SDK trait cannot express the new write path and is replaced.
3. **Materialized effective artifacts** — `resolved_schema`, `effective_traits`,
   `effective_traits_schema`, `resolution_fingerprint` are computed at admission and
   stored, so the P1 read path needs no later backfill.

**Users:** platform and domain gears (via `ClientHub`), operators and CI (via REST).

**Success looks like:** a gear registers its Type Schemas at startup, the process is
restarted, and every entity — with its resolved artifacts — is still there, identical,
without re-registration.

---

## 2. Scope

### In

| Capability | DESIGN reference |
|---|---|
| Durable storage of managed entities, revisions, current-state projections | §3.7 |
| Asynchronous admission: `202` + operation + `Idempotency-Key` + outbox + worker | ADR-0012, §3.2 *Acceptance path* |
| Dependency-aware partial admission (topological order over an acyclic relation) | §3.2 |
| Optimistic concurrency on the logical entity (`resource_version`) | ADR-0005, ADR-0006 |
| Immutable retained revisions | ADR-0005, ADR-0006 |
| Version families: ownership row, kind/shape/contiguity rules | ADR-0004 |
| Dependency edges (`$ref`, derivation, instance_of) — written **and** read | §3.2 *Dependency Graph* |
| **Upgrade `gts-rust` 0.11.0 → 0.12.0** across the workspace (§7) | `constraint-gts-implementation` |
| BACKWARD compatibility against one baseline, tri-state verdict, undecided **rejected** | ADR-0003, `principle-fail-closed` |
| Per-level content-model classification as a compatibility input, reported by Dry Run | ADR-0003 |
| Derivation-chain validation, Draft-07 dialect gate, ADR-0015 major-0 quarantine | ADR-0014, ADR-0015 |
| Dependent revalidation + effective-artifact refresh via reverse worklist | §3.2, §4 |
| Deletion with `expected_resource_version`, blocked by direct registered dependents | §3.2 |
| Lifecycle `ACTIVE` / `DELETED`, tombstones retained | ADR-0008 |
| Dry Run as a mode of registration and deletion | `fr-dry-run` |
| New SDK trait + REST surface for the above | §3.3 |
| Three backends: SQLite, PostgreSQL, MySQL | `constraint-multi-backend` |

### Out — deferred, not redesigned

| Deferred | Why out |
|---|---|
| Tenant ownership, visibility, tenant plane | User decision. Columns are kept, never populated with scope=2 |
| `PlatformSecurityContext` in the contract, a separate platform listener, `PlatformIdentity` enforcement | User decision, and the platform does not offer either to an in-process gear yet (§8.4, C8). The platform-plane **API itself** — SDK trait, async REST, global-entity reads and writes — is **in** P0; only the identity and the listener are deferred |
| PDP / `PolicyEnforcer`, read & write grants, declared permissions | Depends on the deferred identity-to-permission binding (§4 DESIGN) |
| Federation: `source_claim`, the `routing` coordination state, Registry Source Plugins, Control-Plane Validator | Whole subsystem |
| Availability Evaluator, `tenant-resolver` dependency | Needs tenancy |
| Validator inputs that only a tenant or external read has: subject visibility-chain version, Context Tenant availability-chain version, routing generation, `external_revision` | Each is `tenant plane only`, availability-conditional or external, so **none participates in a platform-plane read** — the validator itself is in P0, see §8.5 |
| Arbitrary `$select` projections | The **default** field set is in P0 — that is what makes discovery content-free (§10.2). Caller-chosen sets additionally need optional fields across the models and a normalized field-set digest inside the validator, and buy nothing while there is one representation to select from (ceiling C7) |
| `expand_type_filter` and `limits.expansion_references` | Its DESIGN definition *is* `$select=gts_uuid&availability=available`, with the availability filter fixed by the method rather than supplied by the caller. Availability is out of P0 (needs tenancy), so a P0 method of that name would report retired contracts as usable — a same-named different meaning, which is worse than absence. Paging `list_entities` directly is available to any caller that wants the traversal |
| Operator purge job, operation-retention sweep | ADR-0013, §3.2 — no P0 consumer |
| Aliases, Validation Hooks, casting, tenant enablement | P2 in DESIGN |

### Explicitly *not* simplified

Input validation at the admission boundary, idempotency uniqueness as a database
constraint, and compare-and-swap on every update stay in full. They are the
correctness core, not scope.

---

## 3. Decisions locked in review

| # | Decision | Consequence |
|---|---|---|
| D1 | **Async write path per DESIGN** — `202` + operation UUID + polling | `operation` / `operation_item` tables, `toolkit-db` outbox, admission worker. Kept as a contract that does not break when revalidation later becomes genuinely unbounded, even though the P0 worker completes in milliseconds |
| D2 | **A transient `gts-rust` store per admission unit**, built from the database; reads are served from the database | Resolution, compat, chain validation and derivation need a `GtsStore`, so one is built from the unit's dependency closure and dropped after the unit. Reads need rows, not a store (§8.2), so nothing is held between units. Commit re-verifies under locks |
| D3 | **Materialize effective artifacts** | `type_schema` current-state row is populated at admission. Read path shape identical to P1, no later backfill of `resolution_fingerprint` |
| D4 | **Multi-pod** | Commit transaction re-reads `resource_version` of the candidate and the revision vector of everything consumed, and re-derives the reverse-impact set from the database; a difference rolls back and revalidates within `worker.max_revalidation_attempts`. The comparison is **not** taken under locks on the compared rows — §8.1 step 4.2 argues why locking the vector is the wrong tool and what serializes those rows instead. Ordering is provided by the **`entity_write_order` row** of `types_registry__coordination_state`, advanced as each commit transaction's first statement: one commit at a time per installation, which is what lets the in-transaction scan and guard see each other's work. A row rather than an advisory lock because advisory keys live on a separate session that can be lost while the transaction carries on. Every writer of entity state must claim it — deletion at T20, the purge job under ADR-0013 |
| D5 | **Reverse impact by one scoped recursive CTE, refresh by an iterative loop** | `DependencyRepo::reverse_impact` is a single `WITH RECURSIVE` over `dependency` through `SecureCteSelect` (ADR-0001, `toolkit-db`) — no raw SQL — depth-capped at `limits.activation_write_set`, which is also the refusal threshold for the set it returns. The refresh stays a domain loop because the fingerprint-stability stop decides the write set by recomputation, which no closure query can express. See [§D5](#d5-splits-the-traversal-from-the-refresh) |
| D6 | **The old `TypesRegistryClient` is removed in P0**; every consumer migrates inside this effort | ~50 call sites across 20+ gears move. Forced by two facts: async admission makes the old synchronous `register()` a lie in its own signature, and the old models' `Arc`-linked object graphs cannot cross a wire, so keeping them keeps an out-of-process blocker. Migration is split by gear group — see `plan.md` P5 |
| D7 | `operation.plane = 1` (platform), `tenant_id = NULL`, `principal_id` a hardcoded constant with a `TODO` | Idempotency scope becomes global — see §9 ceiling C2 |
| D8 | **`gts`, `gts-id` and `gts-macros` are pinned at 0.12.0 and move together** | A split pin puts the identifier crate and the semantics crate on different specifications. `gts-dylint` / `gts-macros-cli` must not lag either — see §7 |
| D9 | **Use `toolkit-db/preview-outbox`** | Closes DESIGN §4's outbox sign-off for this gear |
| D10 | **`POST /entities` breaks**: `200` + results becomes `202` + operation | No compatibility path on that route. The gear's REST stability is `unstable`; the break is called out in the changelog |
| D11 | **Registration moves from registry-side pull to per-gear push** | A gear's code must not depend on whether it runs in-process; pull silently loses the types of an out-of-process gear. types-registry seeds only what it owns; every other gear reconciles its own through one SDK helper that owns batching, idempotency and retry. Requires `owning_gear` on the inventory records. See `plan.md` P4 |
| D12 | **`GET /entities` becomes a bounded content-free page with a cursor** | The current shape returns every match with its full `content`; with D3 that is *entity count* × up to 1 MB in one response, and the count is now every gear's declarations. DESIGN specifies this route as content-free discovery returning one page and a cursor. A `limit` without a cursor would bound the response by making the endpoint incomplete, so both land together (§10.2) |

---

## 4. Two DESIGN prerequisites this closes with numbers

DESIGN §4 lists eight implementation prerequisites. P0 closes two of them and must
record how.

**A per-transaction lock timeout in `toolkit-db`.** The write path serializes on the
`entity_write_order` row (§8.1), so every commit queues on a database row lock whose wait only the
backend bounds — and `PostgreSQL`'s default is unbounded. The gear cannot bound it itself: it
issues no raw SQL outside migrations, and `tokio::time::timeout` around the claim is not a
bound, since cancelling the future leaves the statement running and the rollback queues behind
it. What closes this is `TxConfig` gaining a backend-aware lock timeout — `SET LOCAL
lock_timeout` after `BEGIN` on `PostgreSQL`, the session variable set and restored on `MySQL` —
together with contention classification for `55P03` and `1205` so the refusal keeps a shape a
client can branch on. Until then the bound is a deployment setting and this gear documents it
as one.

**Unbounded activation write set.** DESIGN asked for either a permitted
transaction-size/timeout profile for that atomic write or a generation/staging protocol,
"short transaction" alone not being a bound. P0 answers with the first, and DESIGN §3.2
now carries the bound as deployment configuration while §4 keeps only the staging upgrade
path open.

Measured over every chained GTS identifier declared in the repository, the largest
reverse-impact set of any base type is **27** (`gts.cf.core.events.event_type.v1~`; next:
`topic.v1~` 21, `event.v1~` 18, `errors.err.v1~` 14). A revision of the hottest base type in
the platform therefore refreshes ≤ 28 `type_schema` rows in one transaction. The bound below
is set against **27**, not against the size of the declared population — a re-count of the
latter does not move it.

**P0 profile: single transaction, no staging.** Configured bound
`limits.activation_write_set` = **512** entities in the reverse-impact set — the set the
walk returns, before the fingerprint filter decides which of them are rewritten. The
written set is a subset of it, so one number bounds both, and it is the walked set the
bound is *checked* against because that is the number the commit knows before it writes.
Exceeding it fails the candidate with a structured reason rather than committing a partial
refresh. Upgrade path when the bound is reached: the generation/staging protocol DESIGN
describes. Not built.

**Parameterized recursive CTE in `sea-query`.** **Closed, and by verification rather
than by dissolution.** `toolkit-db`'s `SecureCteSelect::recursive_cte` (ADR-0001) builds a
scoped `WITH RECURSIVE` through the typed builder with no raw SQL, and the emitted shape
executes on all three backends — `toolkit-db`'s own `cte_shapes_are_valid_sql_{sqlite,
postgres,mysql}` plus this gear's `reverse_impact_walks_back_up_a_chain`, which runs in
the `PostgreSQL` and `MySQL` container suites.

Two of DESIGN's constraints on the query are honoured as stated and one is not. `UNION`
rather than `UNION ALL`: kept, and it is the builder's default. No per-row accumulator
that would defeat deduplication: **not** kept — the builder emits a mandatory depth
column, because a cap has to be expressible as a predicate on the recursive member. The
consequence is that a node reached at two depths is expanded twice, which bounds
re-expansion at *(rows × depth)* rather than by path count, and the cap is what makes it
finite. `database.sql`'s early stop is unaffected and still sanctions the refresh loop:
*"The traversal reaches the subject anyway, recomputes it, finds an identical digest,
and stops there."*

The `toolkit-db/preview-outbox` sign-off is closed: outbox support is unconditional in
`toolkit-db` 0.12.0. Still open and inherited: worker liveness bounds. The ADR-0015
quarantine preflight is **satisfied by construction** — see §17, O4.

---

## 5. Tech stack

Rust edition 2024, toolchain ≥ 1.95.0. One new third-party dependency: `lru`, for the
store-build cache. Everything else comes from the workspace.

| Concern | Choice |
|---|---|
| GTS semantics | `gts` / `gts-id` / `gts-macros` **0.12.0** — **sole** source, no local approximation (`constraint-gts-implementation`). Upgrade from 0.11.0 is part of this task, §7 |
| Persistence | SeaORM via `toolkit-db` `DBProvider`, `sea-orm-migration` |
| Async dispatch | `toolkit-db` outbox, leased mode, table prefix `types_registry_outbox` |
| REST | Axum via `OperationBuilder`, utoipa, RFC-9457 problem details |
| Errors | `toolkit-canonical-errors` `CanonicalError`, one `From<DomainError>` ladder |
| Shared state | none held between admissions — reads go to the database, the `gts-rust` store is transient per admission unit (§8.2, D2) |

New dependencies for `cf-gears-types-registry`: `toolkit-db`, `sea-orm`,
`sea-orm-migration`, `time`, `aws-lc-rs` (the FIPS-validated SHA-256 the platform
installs at bootstrap, in place of the DE0708-banned `sha2`), and `lru`. The `toolkit`
outbox needs no feature: it stopped being a preview in toolkit-db 0.12.0 and is now
unconditional.

Gear capabilities change from `[system, rest]` to **`[system, db, rest, stateful]`**
(`stateful` for the outbox worker lifecycle). Precedent: `credstore` runs
`[system, db, rest, stateful]`.

---

## 6. Commands

```bash
make dev                 # fmt + clippy autofix + tests — the daily loop
make ci                  # full gate: fmt, clippy, test, deny, dylint, lychee

cargo test -p cf-gears-types-registry
cargo test -p cf-gears-types-registry-sdk
make test-types-registry-db  # this gear's PostgreSQL + MySQL container suites

make dylint              # architecture lints — DE01xx/DE02xx/DE08xx apply here
make e2e-local           # REST surface end to end
```

Definition of done for any slice: `make ci` green, the plain gear tests green on SQLite,
and `make test-types-registry-db` green on PostgreSQL and MySQL. SQLite-only is not
sufficient — `constraint-multi-backend` is a correctness requirement, and the lock and CAS
paths differ per backend.

---

## 7. The `gts-rust` 0.12.0 upgrade

Published `gts` 0.11.0 was missing three capabilities DESIGN §4 requires, and
`constraint-gts-implementation` is categorical that a missing behaviour is *"a change
request against `gts-rust`, not a local approximation."* **0.12.0 supplies all three**,
so the upgrade is a scope item of this task rather than a follow-up, and no deviation
has to be recorded.

Verified against `gts-rust` 0.12.0:

| DESIGN §4 prerequisite | 0.11.0 | 0.12.0 |
|---|---|---|
| 1. **Tri-state verdict**, undecided distinct from incompatible | ❌ bare `bool` | ✅ `CompatibilityVerdict::{Compatible, Incompatible, Unknown}` |
| 2. **Per-level content-model classification** after resolution | ❌ absent | ✅ `ContentModel::{Open, Closed, Partial}`, `classify_object_levels() -> Vec<ObjectLevel>` |
| 3. **Partially open level reported as such**, not forced into a verdict | ❌ absent | ✅ `ContentModel::Partial`; `is_evolvable_in_place()` is `false` for it *"rather than guessed"* |
| 4. **Per-content-model property add/remove**, both directions | ❌ absent | ✅ `CompatibilityFinding::{PropertyAdded, PropertyRemoved, …}`, directional `check_backward_diagnostics` / `check_forward_diagnostics` |
| 5. **Checker spec + impl versions**, persisted on admission | ⚠️ | ✅ `GTS_SPECIFICATION_VERSION = "0.13"`; impl version is the crate version |
| 6. **Document-level comparison that resolves both sides** | ❌ compared unresolved | ✅ `GtsStore::compare_documents() -> SchemaComparison` resolves both, then compares |
| 7. **Registration-policy matching properties** | ✅ | ✅ pinned by the three `gts-id` tests DESIGN cites by name |
| 8. Pattern containment for Source Claim overlap | — | out of P0 scope (federation) |

Three things this settles.

**`SchemaComparison` is the entry point.** DESIGN §3.2 asks the Compatibility policy to
*"compare resolved effective schemas through the document-level compatibility entry
point and reject an indeterminate verdict."* That is `compare_documents` exactly:
it calls `resolve_schema_refs` on both sides and returns `backward_diagnostics`,
`forward_diagnostics` and `candidate_object_levels`, with verdicts recomputed from their
evidence rather than stored beside it.

**`Unknown` must be rejected.** 0.12.0's own doc comment says *"The caller, not this
library, decides how that affects admission."* P0 decides: `Unknown` fails the candidate
with a distinct structured reason, separate from `Incompatible`. That is
`principle-fail-closed` — *"undecidable compatibility is rejected"* — and it is now
implementable rather than a ceiling.

**`candidate_object_levels` is what Dry Run reports.** ADR-0003 wants the level that
prevents admission, and `ObjectLevel` carries the path (`$`, `$.payload`, `$.items[]`).
One document-wide flag would not do — as 0.12.0 notes, in the closed-envelope shape the
deciding level sits inside an extension container, not at the root.

**Do not use `GtsStore::is_minor_compatible`.** In 0.11.0 it compared `old_ent.content` /
`new_ent.content` — the *unresolved* authored documents, violating prerequisite 6. Use
`compare_documents`.

### Upgrade risk: the API is additive, the semantics are not

The `pub use` surface is additive — `schema_evolution` and `SchemaComparison` are added,
nothing is removed. The symbols this monorepo actually uses are a narrow, stable set
(`GtsId::new`/`try_new`/`to_uuid`, `GtsTypeId`, `GtsInstanceId`, `GtsIdPattern::try_new`,
`GtsConfig`, `GtsOps::add_entity`/`validate_entity`, `validate_schema`, `try_narrow`),
and nothing in the repository calls the two moved/deprecated compatibility helpers. The
mechanical part of the upgrade is therefore small.

The behavioural part is not. Between 0.11.0 and 0.12.0: *"fix: Align schema compatibility
with JSON Schema semantics"*, *"fix: correct directional schema compatibility checks"*,
*"fix: localize unprovable schema intersections"*, *"fix(traits): Stop materializing
const values"*, *"fix(macros): Preserve explicit additional properties models"*,
*"fix(gts-id): distinguish v0 from wildcard versions"*.

Two of these reach beyond Types Registry. The macro fix changes generated schema
documents, so `#[gts_type_schema]` output may differ; the traits fix changes what
`x-gts-traits` materializes. **Every declared GTS identifier in the repository must be
re-validated under 0.12.0 before the upgrade lands** — this is T1, and it is the one task
whose failure mode is other gears rather than this one. The `gts-id` v0 fix
works in our favour: it makes the ADR-0015 major-0 quarantine (§8.1 step 7) exact.

`gts`, `gts-id` and `gts-macros` move together — the workspace pins all three, and
`gts-dylint` / `gts-macros-cli` must not lag.

### Sourcing it (D8)

The workspace pins all three crates from crates.io:

```toml
# Cargo.toml
gts-id     = "0.12.0"
gts        = "0.12.0"
gts-macros = "0.12.0"
```

**Verification of the upgrade:**

| Check | Result |
|---|---|
| `cargo check --workspace` | clean, no errors |
| `cargo test -p cf-gears-types-registry` | 209 passed, 0 failed |
| `cargo test --workspace` | **10260 passed, 0 failed, 611 ignored** (most ignores need Docker/testcontainers) |
| `make gts-docs` | 798 files, 0 errors |
| `Cargo.lock` | `num-cmp` is the one transitive dependency 0.12.0 introduces |

Toolchain is not a problem: `gts-rust` requires 1.95.0 and this workspace is already on
1.97.0 with `rust-version = "1.95.0"`. (CLAUDE.md's "minimum toolchain 1.92.0" is stale.)

---

## 8. Architecture

### 8.1 Write path

```
POST → acceptance (synchronous, reads no entity state)
     → one transaction: insert operation + operation_item(s) + enqueue outbox(operation UUID)
     → 202 + operation UUID + Location + Retry-After
                                            ↓
                          leased outbox → admission worker
                                            ↓
     transient store from the unit's dependency closure → evaluate (outside any transaction)
                                            ↓
        per admission unit: short transaction with database rechecks + writes
```

**Acceptance checks, in this order** (DESIGN §3.2; steps 4 and the tenant half of 3 are
out of scope):

1. Envelope and batch size — refuse > `limits.batch_candidates` (100).
2. Candidate identifiers — refuse non-canonical GTS identifier, or duplicate within batch.
3. Registration policy — for a declared creation, the candidate's last-segment vendor
   must be admitted for its region (`allowed_vendors`; the region's `tenant_ownable`
   parameter is inert in P0 — §10.3). Closed by default; global `cf` is implicitly
   admitted. Revisions and deletions bypass this gate.
4. Managed identifier profile — refuse an explicit UUID tail (ADR-0001); refuse a minor
   or major 0 in the **last** segment of an Instance identifier (ADR-0004, ADR-0015).
   A minor on a Type Schema identifier is admissible under any prefix.
5. Declared dialect, Type Schema candidates — top-level `$schema` present and in the
   closed Draft-07 spelling set; any `$schema` below the root must not differ (ADR-0014).
6. `force` per candidate — refuse where `allow_compatibility_force` is off, or where the
   candidate has no cross-minor check to waive. Until T17, also refuse every surviving
   `force`: there is no comparison to waive and no truthful `compat_forced` to record yet.
7. ADR-0015 quarantine — refuse a stable candidate whose immediate base or `$ref` targets
   include a major-0 identifier. `x-gts-ref` is outside the quarantine.
8. Canonicalize through `gts-rust`, compute the request fingerprint, resolve the
   mandatory `Idempotency-Key`.

Ordering invariant that must not be reordered: step 3 precedes any existence lookup, so
a refusal cannot probe the namespace. Steps 4 and 6 are request-static; family shape and
whether a waived comparison would fail remain worker decisions inside the commit transaction.

Replay of a matching fingerprint under the same key returns the stored operation —
`202` while active, `200` when terminal. A different fingerprint under the same key
returns `409`.

**The worker is a plain function, not a task.** Its entry point takes
`(operation_id, runner)` and performs one full pass; the outbox handler is a thin shell
that calls it and maps the result to `Ok` / `Retry` / `Reject`. This is required by the
testing rules of §13 — no test may poll — and it is what makes every concurrency case
below reachable in a `#[tokio::test]` against SQLite `:memory:`.

**Where it runs.** The outbox worker is started at the end of types-registry's own `init()`,
not in the stateful `start` entry, wired to `ctx.cancellation_token()` and stopped through the
retained `OutboxHandle`. `init` of every gear precedes `start` of any, so a worker in `start`
would leave operations submitted during a consumer's `init()` sitting `pending`. Startup order
inside `init()` is: repositories → inline seeding → start worker → publish client. There is no
snapshot step and no warm-up read: seeding builds its own transient store like any other
admission (D2), and every later read goes to the database.
Seeding precedes the worker start and enqueues nothing, so seed operations cannot be leased
concurrently.

**Two seed sources, one inline pass.** Seeding covers (1) types-registry's own toolkit-gts
inventory (base types and control-plane types it declares) and (2) the operator-configured
`cfg.entities` from the deployment YAML — identities whose GTS identifiers are
deployment-specific and cannot be expressed as gear-owned inventory items (e.g. the
platform-root tenant type whose identity is chosen by the operator). Both sources are admitted
together in a single inline pass; an invalid or oversized combined seed set fails startup
loudly. D11 governs (1): types-registry seeds only what it owns and every other gear reconciles
its own through the SDK helper. `cfg.entities` is outside D11's scope — it is not owned by any
gear and is not reconciled through the SDK; it is deployment configuration that the registry
admits on behalf of the platform operator.

Acceptance and admission therefore have different executors. Acceptance is always
synchronous, in the caller's task, inside registry code: the REST handler for API traffic, or
the local client for an in-process SDK caller. Admission is performed by exactly one outbox
worker owned by types-registry — one in the system for a single-binary deployment. Seeding is
the exception both ways: types-registry accepts and admits it itself, inline, with no outbox.

**Worker, per admission unit:**

1. Build the candidate graph. Two edge sets, and they are not the same set:
   - the **ordering** graph is authored `$ref`s between candidates, each candidate's
     identifier-derived immediate derivation base, its Instance conformance target, and the
     implicit `vM.(n-1)~ → vM.n~` edge (not stored in `dependency`). Conformance cannot close
     a cycle and is still needed here: an Instance must not commit ahead of the Type Schema
     that may then be refused;
   - the **cycle-bearing** graph is `$ref` and derivation. Those are the two an effective form
     inlines, so those are the two a cycle can be built from.
2. Process in topological order; one candidate is one unit. The *admitted* relation is
   acyclic (ADR-0012): derivation strictly shortens the `~`-chain, nothing references an
   Instance, and every cycle over the combined `$ref`-and-derivation edge set is refused
   because it has no resolved form. The in-batch graph is not acyclic by construction — the
   overlay lets candidates see each other — so the ordering detects a cycle and fails its
   members with `invalid_schema`. Past that refusal there is no condensation step and no
   atomic group.
3. Build the unit's transient `gts-rust` store (D2): the candidates, plus the transitive
   closure of what they consume, read `gts_id`-sorted from the database. Evaluate outside
   any transaction against it: resolution, compat vs
   baseline, derivation, references, dependent revalidation. Record the target revision,
   the reverse-impact identifier set, and a revision vector (`resource_version`, plus
   `resolution_fingerprint` where effective content was consumed) — together with the
   **closure roots** the reads were driven from, so step 4.3 re-derives the same question
   rather than merely re-reading the same rows. The vector's reads run in the **same**
   transaction as the store build, so the state it records is the state the documents
   being validated came from; only the validation itself is outside a transaction.
4. Commit transaction:
   1. **claim the `entity_write_order` row.** This is the transaction's first statement and
      nothing may precede it, reads included: every step below is an answer about
      committed state, and a read taken before the claim is an answer about state that
      can still move. It is what orders commits, and the order is total only because it
      comes first. No other lock is taken — T15 retired the family advisory locks, and
      the reasoning is below;
   2. enforce the caller precondition — creation requires the identifier absent, update
      requires `entity.resource_version == expected_resource_version`. An update whose
      authored content equals the current revision's is **`unchanged`** and terminates
      here: it writes no revision, moves no version and refreshes no dependent, so the
      guard below does not apply to it and is not asked;
   3. re-derive the revision vector from the database — the dependency closure from the
      same roots evaluation used, and the reverse-impact set (D5) — and compare
      **membership and every column**: `resource_version` throughout, plus
      `resolution_fingerprint` for each dependent, whose artifacts a refresh moves
      without moving its version. Any difference rolls the transaction back and
      revalidates from scratch, bounded by `worker.max_revalidation_attempts`;
      exhaustion terminalizes the item as `failed` with reason
      `revalidation_exhausted`;
   4. re-test predecessor existence for each minor-bearing candidate;
   5. insert the immutable revision, replace the current-state projection, replace the
      entity's outgoing dependency edges;
   6. refresh affected current effective schemas (bounded by `limits.activation_write_set`);
   7. increment `resource_version`, record the outcome and resulting version.

**Why step 4.2 takes no row locks over the revision vector.** The step was written as
*"then lock candidate and revision-vector entity/current rows in canonical identifier
order"*, and P0 does not do that. Not because it cannot — though it also cannot, see
below — but because locking the vector is the wrong tool for what the vector is:

- A lock guarantees only that nothing moves **after** it is taken. Movement between
  evaluation and the moment of locking is precisely what step 4.3 exists to catch — the
  phantom dependent appears before any lock could be held — so the comparison is required
  either way and the lock is purely additive. What it would buy is that a contended
  admission *waits* rather than rolling back: liveness, not correctness.
- It costs one round trip per vector member inside the commit transaction, on a set
  `activation_write_set` allows to reach 512 — the cost T14 restructured the reverse read
  into a single CTE to avoid. And it is the sole reason the canonical ordering in step 4.2
  has to extend past families at all: order matters because the locks are many.
- It is the wrong shape for this design, which is optimistic throughout —
  `resource_version` compare-and-swap, a transient store per unit, validation outside any
  transaction. Registrations are rare against reads, which is the regime optimistic
  detection is for.
- Only then, the platform fact: the secure query API exposes no `FOR UPDATE`
  (`toolkit-db/src/secure/select.rs`) and `SQLite` has no row locking at all, so the
  portable primitive would be the advisory lock. This is corroboration, not the reason —
  the argument above would stand if `FOR UPDATE` were available tomorrow.

**What holds instead, and why it is sufficient.** The candidate's own row is serialized by
the compare-and-swap that writes it: the precondition travels in the statement's `WHERE`,
so there is no gap between checking the version and moving it. A **dependency** that moves
is serialized by the refresh its mover owes the dependants (D5): that refresh writes each
affected dependant's `type_schema` row — the same row this commit writes — so the two
block on one another in the database. The block orders them; what makes the loser *notice*
is the compare-and-swap described next, not the block. Where the mover's change leaves a
dependant's `resolution_fingerprint` unmoved it writes nothing and no conflict arises, and
that equality is itself the proof that nothing this commit consumed went stale.

**The row conflict orders the writes; it does not recompute the loser.** That is worth
stating precisely, because the earlier form of this paragraph claimed it did. Artifacts are
computed in Rust and only then written: the row lock makes the second writer wait, and the
wait re-evaluates the predicate, not the payload. A write that lost the race would therefore
apply artifacts computed before the winner landed — and pair them with a `revision_no` read
afterwards, which is exactly the row `update_current`'s contract forbids.

So **every artifact write carries a compare-and-swap** on `(revision_no,
resolution_fingerprint)`, and a miss is drift (`current_projection_moved`): the evaluation is
void and the retry redoes it, under `worker.max_revalidation_attempts` like every other drift.
What the token *is* differs between the two writing paths, and the difference matters:

- **The refresh** recomputes each dependent inside this transaction, so its token is the
  projection state those artifacts were computed against — taken from the read that selected
  the document, never from a later one, because a later read would adopt whatever moved in
  between and confirm it.
- **The candidate's own revision** computed its artifacts at *evaluation*, outside any
  transaction, so no token could be their input. Its token is read inside the commit, before
  step 4.3, and is a **post-guard sentinel**: the guard is what establishes that the
  evaluation's view still holds, and the sentinel is what establishes that nothing wrote this
  row between the guard and the write. Composition is the argument — token read, then guard,
  then swap — not a claim about where the artifacts came from. It is needed because the entity
  compare-and-swap does not cover this row: a refresh owed by a *transitive* dependency shares
  no lock with this commit and writes exactly here.

Only a write whose artifacts cannot be stale goes unconditional, which in P0 is an Instance,
having no artifact row at all.

**Both compare-and-swaps are unreachable while commits are serialized**, and are kept anyway.
Nothing can move a row under a commit when no second commit is in flight, so neither swap can
lose. They are depth, not the mechanism: they are what a narrower ordering protocol would rely
on if the global lock is ever replaced, and they cost one predicate on a statement that is
issued regardless. Their known limit belongs with them — the swap is
`(revision_no, resolution_fingerprint)` rather than a monotonic version, so an `ABA` return to
an identical fingerprint would defeat it. Under the lock that is unreachable; a protocol that
relaxes the lock has to introduce the monotonic version with it.

What step 4.3 buys on top of the lock is that an evaluation is never committed against a state
it did not see, which is what makes the transient-store design (D2) safe.

Canonical identifier order is kept where it is observable. P0 takes no family locks, so what
remains observable is the vector: it is `(gts_id, role)`-sorted on both sides, which is what
makes the comparison one merge walk and the drift it reports deterministic rather than row-order
dependent. `domain::family::lock_order` survives for the writer that needs it next — DESIGN's
per-family ordering, which deletion and purge inherit.

**The liveness cost is real and named.** A dependant of a base that is being revised
constantly can exhaust `worker.max_revalidation_attempts` and terminalize as
`revalidation_exhausted` where a lock-holding commit would have waited and succeeded. The
answer to that is the attempt budget and, if it is ever observed, backoff between attempts
— not locks over the vector. P0 does not observe it: the only writer is the registry's own
seeding, and the retry arrives under the caller's `Idempotency-Key`. The commit ordering below
adds a second cost of the same kind: a unit whose commit is queued behind another waits on the
database, and if the backend's own lock timeout cuts that wait, the result is a transient
storage error — contention, never a verdict on the candidate, recovered by a replay under the
caller's `Idempotency-Key` until T21 wires the outbox and by redelivery after it.

**The write path is serialized, and that is a correctness requirement.** The optimistic
mechanisms above cover everything that meets on a row: a dependant that already exists is
reached by the mover's refresh and one of the two compare-and-swaps loses. What they cannot
cover is **reachability the mover's reverse scan did not see** — an edge committed after that
scan, whether by an entity that did not exist when it ran or by an existing one adding a
reference. Adding an edge writes only `dependency` and moves no `resource_version`, so the two
commits write no row in common and no swap can lose. Both pass their own guards, and the
dependant keeps an artifact inlined from a revision that is no longer current, carrying a
`resolution_fingerprint` that matches that artifact and therefore reports no drift. Nothing
later repairs it.

The mechanism that closes it is a **serialized write path**: every commit claims the
`entity_write_order` row of `types_registry__coordination_state` as the **first statement** of
its transaction, so one commit runs
at a time per installation. Both the reverse-impact scan and the vector guard run *inside* the
transaction, so that ordering gives them a total order, and the argument is two cases with no
third:

- **The edge commits first.** It is in `dependency` when the mover claims the row, so the
  mover's scan — running after that claim — finds the dependant and refreshes it.
- **The mover commits first.** The unit writing the edge has committed nothing yet. When it
  does, its own guard re-derives its vector after its own claim and sees the mover's new
  `resource_version` on a member of its closure: drift, rollback, re-evaluation against the
  new revision.

Neither side needs to know about the other. Without the total order both halves can be in
flight at once, each blind to the other, which is exactly the schedule above.

**A row, and not an advisory lock.** `toolkit-db` holds advisory keys on a session separate
from the commit transaction's connection, so losing that session releases the key server-side
while the transaction carries on — mutual exclusion would lapse, and silently, since the
generation epoch is only consulted at release. A row lock ends with `COMMIT` or `ROLLBACK` and
with nothing else, which is the property this rests on.

**First statement, not merely early.** Claimed after any read, the row would order the writes
and leave those reads outside the serialized region — which is the interleaving it exists to
prevent.

**Why the guard is still required, and is not redundant.** Evaluation runs *outside* the
transaction, so a commit that landed between this unit's evaluation and its own claim leaves the
evaluation stale. That is what the vector catches, and it stays the only thing that can:
`a_dependency_mutated_between_evaluation_and_commit_costs_one_rollback_and_one_retry` holds a
pass at its evaluation closure read for exactly that reason. The claim closes what the guard
could not see; the guard closes what the claim does not cover.

**Why the dependency side of the vector needs no fingerprint.** An artifact is a pure function
of the **authored** documents of the closure — `load_unit_store` and the refresh both read
`current_documents`, never materialized artifacts, because those are outputs of the resolution
the store performs (D3). An authored document changes only with a new revision, which moves
`resource_version`. So `resource_version` per closure member is a complete staleness detector
for dependencies, and `resolution_fingerprint` is recorded only for *dependents*, whose
artifacts the refresh consumes directly.

**What it costs, and where it lands.** Admission throughput, and only that: evaluation —
parsing, resolution, compatibility, derivation — stays outside the transaction. What is
serialized is the commit transaction, for its whole length, for **every** outcome: an
`unchanged` re-submission claims the row and writes its item outcome, and a creation writes an
entity, a revision and a current row. Unrelated registrations queue behind both.

**The queue wait is the database's, and the gear states no bound on it.** `lock_timeout` on
`PostgreSQL`, `innodb_lock_wait_timeout` on `MySQL`, `busy_timeout` on `SQLite` — deployment
settings this gear does not own, and an operator running an installation with concurrent
registrants should set the first, since `PostgreSQL` waits forever by default. Exceeding one
surfaces as an ordinary database error, which leaves the operation non-terminal — recovered by
a replay under the same `Idempotency-Key` until T21 wires the outbox, and by redelivery after
it.

A gear-side budget was tried and removed, and the reason is worth keeping: wrapping the claim
in `tokio::time::timeout` bounds nothing. Cancelling the future leaves the statement running on
the connection and the rollback queues behind it, so the caller waits exactly as long and only
gets a different error at the end — the container test written against that version deadlocked
instead of reporting a refusal. A per-transaction lock timeout on `toolkit-db`'s `TxConfig`
(`SET LOCAL lock_timeout` after `BEGIN`) is what would give the gear a bound it can state; §4
records it.

What varies is the expensive part, the dependents' re-materialization the commit performs in a
blocking task while the transaction is open, bounded by `limits.activation_write_set`. That
part *is* zero for `unchanged` and for a creation — the first refreshes nothing, the second has
no dependants yet — so the serialized section is short except when revising something already
depended on, whose measured maximum in the repository is 27 dependents. If the serialized
section is ever measured to matter, the upgrade path is a graph generation compare-and-swapped
at commit, which restores parallelism and subsumes this lock; it is not built, and nothing
depends on it being built.

**Every writer of entity state must claim the row**, or the order it provides is not total.
Today that is this path alone. Deletion (T20) and the operator purge job (ADR-0013) join it,
and each is a correctness obligation of its own task rather than an optimization.
`a_creation_claims_the_entity_write_order_row_exactly_once` and its revision and
`unchanged` siblings are what makes an omission visible: the row is a
monotone count of committed admissions, so a writer that skipped it shows up as a count that
did not move.

**P0 takes no family advisory locks, and this is a deviation from DESIGN §3.7 worth naming.**
DESIGN has the commit lock or create every candidate family in canonical order; T12 implemented
that with advisory keys held across the transaction. The claim retires them for three reasons,
in order of weight:

- They are **redundant**: their whole window — `create_or_get`, the three family rules, the
  entity insert — is inside the commit transaction, which the claim makes exclusive. The
  check-then-act they serialized is already serialized.
- They **invert an order**. Admission took them *before* its transaction, while ADR-0013's purge
  claims the row and then takes family locks. Two writers acquiring the same two things in
  opposite orders is a deadlock, and keeping a redundant lock is a poor reason to have one.
- They **turn a wait into a refusal**. Taken before the transaction, two passes could collide on
  a family key while neither had claimed the row, and the loser answered `503` where it would
  otherwise have queued and succeeded.

What DESIGN describes stays right for the design it describes: a narrower ordering protocol,
one that does not serialize every commit, needs per-family ordering again and would reintroduce
them — together with the entity and routing tiers DESIGN orders after them.

**The write-set bound is asked twice.** `limits.activation_write_set` bounds the
reverse-impact read in the vector as well as in the refresh, so an over-bound candidate
is refused with `activation_write_set_exceeded` at **evaluation**, before a transaction
has written anything, and the refresh's own refusal (step 4.6) remains as the backstop
for a set that grew in between.

**Resolution budgets apply to each document.** Before `gts-rust` resolves a candidate,
`limits.resolution_closure` counts the candidate itself and the distinct documents reached
through `$ref`, derivation and Instance conformance in the candidate-overlaid store.
Converging paths count once; `x-gts-ref`, removed outgoing references and unrelated documents
loaded for other refresh subjects do not count. Exceeding the budget yields
`resolution_closure_exceeded`. Each canonical effective artifact must also fit
`limits.resolved_document` UTF-8 bytes; exceeding it yields `resolved_document_too_large`.
An Instance's conforming schema is subject to the same byte limit. Both checks also apply
to each refreshed dependent, and a refresh refusal rolls back the candidate revision and
all dependent writes. Zero is invalid for either configuration setting.

These are composition and output bounds. The repository's separate 512-entity store-build
guard still bounds database loading, and `gts-rust` constructs the resolved value before its
canonical byte size can be checked; the byte limit is not an allocator-level memory cap.

Deletion has its own short protocol, and it opens the same way: a transaction whose **first
statement** claims the `entity_write_order` row, then the positive `expected_resource_version`
precondition, the recheck that the target is `ACTIVE` at that version with no direct
registered dependents, `DELETED`, the version increment, the outcome. The claim is not
optional and it is not merely early — a deletion whose recheck runs before it is a
check-then-act on state that can still move, which is the failure the recheck exists to
prevent.

Dry Run follows the same path in a rollback-only evaluation transaction, then records
the predicted outcome in a separate short transaction.

### 8.2 Read path, and why no store is held between admissions

**Reads are served from the database.** Nothing persistent lives in process memory.

This is not the obvious choice, and it rests on separating two questions that look like
one: *what needs a `GtsStore`* and *what needs entity rows*. They have different answers.

A `GtsStore` is needed only where GTS semantics are computed over a set of related
documents — `resolve_schema_refs`, `compare_documents`, derivation-chain validation,
instance validation against a type. All of that happens **inside** admission, on one
candidate plus what it consumes.

Reads need rows. The exact-read primitive is a keyed lookup, and the list primitive is
identifier matching that `gts-id` already implements as a pure function on the identifier
string — the current `InMemoryGtsRepository::list` iterates the store purely as a row
container and filters with `GtsIdPattern`, never asking the store a semantic question. And
by D3 the effective artifacts a reader wants (`resolved_schema`, `effective_traits`,
`effective_traits_schema`) are already materialized on the current-state row. So a read is
a `SELECT` plus, for a pattern query, `GtsId::matches_pattern` over the candidate rows in
Rust. No GTS semantics are reimplemented, so `constraint-gts-implementation` is not
touched.

**Why the process-local snapshot was rejected.** A snapshot rebuilt after each local
admission unit cannot satisfy the multi-pod read criterion of §13 — *"two pods, commit on
A, B's first post-commit read sees it"* (`nfr-multi-pod-correctness`). P0 has no
invalidation channel between pods: no pub/sub, and the outbox is the committing pod's own.
Pod B would serve its stale snapshot indefinitely. Admission is protected against exactly
this by the commit-time revision-vector guard (D4, §8.1 step 4.3), which makes evaluation
against possibly-stale data safe; **reads have no such guard**, so for them staleness is
simply wrong. Serving reads from the database makes the criterion true by construction
rather than by a mechanism P0 does not have.

Two consequences worth naming, because they are the price:

- A read is a database round trip where it used to be a memory read. That is what the SDK
  client cache of §8.3 is for, and it is the reason that cache is kept rather than removed:
  a bounded freshness window on the *client* is a trade DESIGN §3.3 sanctions, whereas a
  process-local store treated as authority on the *registry* side is not.
- Startup no longer reads the whole table, and no `GtsOps` is held for the process
  lifetime. That retires two ceilings rather than deferring them (§9, C1 and C4).

**The transient store, per admission unit.** The worker builds a `GtsStore` from the
database rows the unit needs: the candidates, plus the transitive closure of what they
consume, obtained from the `dependency` table (which D5 already writes and reads). It is
dropped when the unit ends.

The closure is seeded from **both** the candidates' identifiers and their documents. The
identifier supplies the `~`-chain — every derivation base and an Instance's conforming type
— and the document supplies its `$ref` targets, which no identifier implies. An
`x-gts-ref` target is seeded by neither, because validating that keyword never reads the
target document. The document half is not an optimization: a candidate's own edge rows are written
at commit (step 4.5 above), so during the read that validates it they either do not exist
yet, on a first admission, or still describe the previous revision. A reference target that
no entity carries is reported apart from a candidate that has no row yet — the second is
the ordinary state of every creation, the first is a fact about the registry. Rows are loaded `gts_id`-sorted, so a derived schema never
loads before its base — lexicographic order on GTS chain identifiers already implies
parent-before-child, since a base identifier is a strict prefix of every identifier
derived from it, as the existing `switch_to_ready` documents.

Building from the closure rather than from every row is what keeps this cheaper than the
snapshot it replaces: the snapshot cost a full rebuild after every successful unit,
whereas the closure is bounded by the candidate's own dependencies.

`Mutex<GtsOps>` disappears with it. The current code holds one because *"`GtsOps` contains
a `Box<dyn GtsReader>` which is not `Sync`"*; a store owned by one worker invocation and
never shared needs no lock at all, so the non-`Sync` field stops being a design
constraint instead of being worked around.

`temporary` / `persistent` two-phase storage and **ready mode are removed.** They existed
only because there was no durable source of truth and startup ordering was unknown; with
a database supplying entities on demand, neither has a job. `SystemCapability::post_init`
and `switch_to_ready` go away, which also settles DESIGN's objection that the `post_init`
barrier conflicts with `constraint-boot-path`.

---

### 8.3 The SDK client cache stays

DESIGN requires a client cache — `cpt-cf-types-registry-fr-client-cache`, *"bounded
per-client representation cache with batched conditional revalidation and fail-closed
expiry handling"* — and under D2 it earns its keep: a read is now a database round trip,
not a memory read, so the cache buys what it costs elsewhere.

Its bounded staleness window is sanctioned, not a correctness violation.
`cpt-cf-types-registry-nfr-cache-correctness` is *"no invalidated result accepted as
current after the client observes the mutation"*, and DESIGN §3.3 draws the line
explicitly: *"a remote mutation not yet observed may produce a stale snapshot within the
bounded window but is not described as an invalidated entry accepted as current."* That is
also a different NFR from `nfr-multi-pod-correctness`, which constrains the **registry's**
own reads — *"no process-local authority"* — and is satisfied by D2. A client's freshness
window and registry-side authority are not the same property.

What P0 builds is DESIGN's cache minus what needs inputs P0 does not have:

| DESIGN §3.3 | P0 |
|---|---|
| Bounded store, LRU eviction | ✅ — bound is **bytes**, not entries: §3.2 caps one resolved document at 1 MB, so today's `capacity: 1024` bounds memory to nothing useful. DESIGN's argument, adopted |
| Freshness window, `0` meaningful and supported | ✅ — DESIGN's 30 s default replaces today's 1 min |
| `fresh` per-call bypass | ✅ — without validators it re-reads from the source rather than revalidating, which is the same guarantee for the caller |
| Invalidation on an observed terminal mutation, across identifier and UUID keys | ✅ — a client observes a mutation when a poll or the reconciliation helper returns a terminal successful outcome, **not** when the `POST` is accepted |
| Entries indexed by both identifier and UUID | ✅ — already true of the current cache |
| `NotFound`, `Failed`, discovery pages and operation resources never cached | ✅ — all four are expressible in P0 |
| Key includes visibility context, Context Tenant, normalized projection | ❌ — no tenancy and no projections in P0. The key carries the fields as fixed markers so P1 adds dimensions without reshaping it |
| Batched conditional revalidation against validators, fail-closed | ✅ — §8.5 puts validators in P0, so expiry sends expired keys and their validators in one conditional `batchGet` and keeps an `unchanged` entry. A failed revalidation propagates and never extends the window |

The cache is not carried over as-is: it is typed on `GtsTypeSchema` / `GtsInstance`, which
D6 deletes, so it is ported onto `EntitySnapshot` when the new trait lands. Until then the
existing cache keeps serving the old trait untouched.

Ceiling C7 records what is still fixed rather than derived: the visibility and projection key dimensions.

---

### 8.4 Out-of-process readiness

Gears may later run out of process, so this records which P0 choices survive that and which
do not. The protocol and the trait are OoP-shaped; the **registration model is not**.

#### Four surfaces, four consumers

DESIGN puts platform-plane operations on HTTP, while the toolkit's out-of-process path is
gRPC. These are not competing transports for one caller — they serve different callers:

| Surface | Transport | Consumer | In P0 |
|---|---|---|---|
| Local client via `ClientHub` | none — a direct call | gears in the same binary | ✅ |
| gRPC service + SDK gRPC client | gRPC through `grpc-hub` | gears in **other processes** | ✗ |
| REST on the business listener, serving **platform-plane** operations | HTTP | operators, CI, e2e, and any non-gear caller registering or reading global entities | ✅ |
| A separate platform listener with `PlatformIdentity` / `X-ToolKit-Internal-Token` | HTTP | the same callers, once the plane is enforced by transport rather than by contract | ✗ (deferred, ceiling C8) |

**Every P0 operation is platform-plane, and the transport does not yet say so.** Registering
a global entity *is* a platform-level operation — §8.1 writes `plane = 1`, `tenant_id = NULL`
on every operation record, and P0 has no tenant-owned entity at all. So the REST surface of
§10.2 is the platform-plane API for global entities: the SDK trait, the async protocol and
the `202` contract are all platform-plane by construction.

What P0 does **not** have is that plane enforced by the transport, and the reason is that the
platform never offers it to an in-process gear:

- `internal_auth_middleware` — the inbound platform plane over HTTP — is installed only in
  `libs/toolkit/src/runtime/oop_serve.rs`, the per-gear HTTP server used when a gear runs
  *out of* process. api-gateway's own `internal_auth` config is an **outgoing** gRPC
  credential for DirectoryService polls (`api-gateway/src/gear.rs:823-826`), not an inbound
  validator, so nothing authenticates a platform identity on the gateway path.
- api-gateway serves one API listener (`bind_addr`); the only second listener it supports is
  for health probes (`HealthServeMode`). ADR-0006/0008 ask for the platform plane on a
  separate listener; no gear or gateway does this today.
- `OperationBuilder` has no `.platform()` and no route policy, and the middleware is
  permissive — a missing token passes through without a `PlatformSecurityContext`
  (`toolkit-http-middleware/src/auth.rs:220`). "Platform-plane only" is not expressible
  declaratively; a handler would have to assert it itself.

Given that, P0 does **not expose mutation routes through api-gateway**. `OperationBuilder`
defaults `exposed` to `false`, and registration and deletion deliberately retain that default
until a platform listener can authenticate a platform principal before dispatch. They stay
`.authenticated()` for internal calls; marking them `.anonymous()` without a platform identity
to put in its place would be a security regression. Non-mutating routes keep their existing
authentication-only posture. This is the same shape as the PDP deviation for reads:
authenticated, not authorized, with the gap named rather than implied (§12, C6). Recorded as
ceiling **C8**.

The future v2-to-v1 path promotion changes paths only. It must not add `.exposed()` to a
mutation route. Publishing registration or deletion is gated on the platform listener plus a
platform-principal/PDP decision; until both exist, those operations are reachable only through
internal inter-gear communication and direct test routers.

**Platform identity in the client is what forces gRPC — expect the move, do not treat it as a
contingency.** A REST contract method whose first parameter is `&PlatformSecurityContext` is
rejected at compile time by the contract macro, with the message *"generated client cannot
source the internal token… serve over gRPC or write a manual client"*
(`toolkit-contract-macros/src/rest_contract_parse.rs:337-347`, UI test
`rest_platform_secctx_rejected.rs`). So P0's client is REST-generatable **because** it carries
no `PlatformSecurityContext`; the moment identity enters the contract, REST codegen closes and
the choice is gRPC or a hand-written client using
`toolkit_http::attach_internal_token_http` — which is exported and has **zero call sites** in
the repository, so that path would be its first user. That is a fact about the toolkit, not a
preference about transports, and it is why §10.1's ctx-less trait has a planned breaking
change tied to OoP rather than only to tenancy.

**Platform REST is not the out-of-process path.** A gear that moves out of process stays a
gear: it resolves the SDK trait from `ClientHub` and the transport beneath it changes from a
direct call to gRPC. Platform REST exists for callers that are not gears — humans, jobs, CI,
external workloads authenticated by `X-ToolKit-Internal-Token` or mTLS SPIFFE. The repository
shows the split: `examples/oop-gears/calculator` carries `proto/`, `client.rs` and `wiring.rs`
in its SDK plus `api/grpc/server.rs` in the gear, and has no REST surface at all, while
`gear-orchestrator` declares `capabilities = [grpc, system, rest]` and carries both.

Two P0 properties make the later gRPC surface cheap rather than a redesign. The async protocol
is transport-neutral by construction — submit, get an id, poll — with no streaming or
callbacks. And `Idempotency-Key` is a trait *parameter*, not an HTTP header: REST maps it onto
the header, gRPC would map it onto metadata or a request field.

**The risk is divergence, not transport choice.** If logic accumulates in REST handlers, a
gRPC adapter added later either duplicates it or drifts from it. The structural guard is the
one the toolkit already prescribes: `api/rest` and a future `api/grpc` are sibling **thin**
adapters over one domain service, SDK models are the single wire vocabulary, and DTOs never
leave `api/rest` (DE0201). Concretely, the domain service's public surface must be sufficient
for a gRPC adapter without adding domain methods — every REST handler is a mapping step only.

| Holds under OoP | Why |
|---|---|
| The new SDK trait | Transport-agnostic — no Axum, no HTTP types, no serde in the SDK crate |
| The async protocol | Submit → operation id → poll. No streaming, callbacks or shared memory |
| Multi-pod correctness (D4) | Already assumes several processes against one database |
| Materialized artifacts (D3) | A remote reader gets everything in one response; the server holds no per-caller state |

**The pull model is an in-process-only assumption, and it blocks OoP.**
`all_inventory_type_schemas()` collects `inventory` records linked into *this* binary. A gear
running out of process declares its `#[gts_type_schema]` types in *its* binary, where
types-registry cannot see them. The pull model does not degrade under OoP — it silently
loses those types entirely. Out-of-process operation is therefore **blocked on the push
migration** — which is why D11 brings that migration into P0 rather than deferring it. Once
T24 lands, this blocker is gone and ceiling C3 is struck.

Two smaller consequences:

- The ctx-less trait (§10.1) is not OoP-shaped: identity must cross the wire where no ambient
  context exists. The planned breaking change that adds `ctx` is tied to OoP, not only to
  tenancy.
- The SDK will need `proto/`, a gRPC client implementation and `wiring.rs` with
  `wire_client()` / `build_client()` per `09_oop_grpc_sdk_pattern.md`. P0 builds none of
  these.

What P0 must preserve is that the trait stays gRPC-expressible. The **old** trait does not:
`GtsTypeSchema.parent: Option<Arc<GtsTypeSchema>>` and
`GtsInstance.type_schema: Arc<GtsTypeSchema>` are in-process object graphs that cannot cross
a wire without flattening. The §10.1 models are flat and hold no shared handles, so replacing
the SDK (D6) also removes an OoP blocker.

---

### 8.5 Freshness validators and conditional reads

**Validators, `ETag` / `If-None-Match` and conditional reads are in P0.** They look like they
need the inputs tenancy supplies; DESIGN §3.3's own input table
(`cpt-cf-types-registry-tech-freshness-validator`) says otherwise:

| Validator input | Managed | Applies to a P0 read |
|---|---|---|
| `entity.resource_version` | ✓ | **yes** — compare-and-swap already maintains it |
| `type_schema.resolution_fingerprint` | ✓, Type Schemas only | **yes** — materialized at admission (D3) |
| subject visibility-chain version | ✓ **tenant plane only** | **no** — DESIGN: *"a platform read has no subject visibility chain"*, and §8.4 establishes every P0 read is platform-plane |
| Context Tenant availability-chain version | ✓, only when availability is selected | **no** — availability is out of scope, and the input is conditional even in P1 |
| routing generation | — external only | **no** — federation is out of scope |
| `external_revision`, `content_hash` | — external only | **no** — Externally Managed Entities are out of scope |
| normalized projection | ✓ | **yes**, as a constant — no caller-chosen `$select` exists (§10.2). One marker suffices because the only two validated surfaces, the exact read and `batchGet`, share one default set; a discovery page carries no validator at all |

The tenant inputs are not missing from P0. They **do not participate** in a platform-plane
managed read, by DESIGN's own rule. So the P0 validator is complete rather than approximate:
a versioned digest over `resource_version`, `resolution_fingerprint` where the kind has one,
and the default-projection marker.

`resolution_fingerprint` is not redundant beside `resource_version`, and this is the case a
simpler digest gets wrong: a dependent's effective schema is refreshed when a base is revised
(§8.1 step 4.6), and §13 requires an identical recomputation to move **no** `resource_version`.
A `resource_version`-only validator would report `unchanged` for a dependent whose resolved
document genuinely changed.

**Computed, never stored** (`principle-derive-not-store`). No column holds a validator, and no
cache holds one as authority.

**Wire form is DESIGN's**: base64url of a versioned JSON object, byte-identical in the `ETag`
header and in batch bodies, 128-bit digest for the managed case. Comparison decodes the fields
rather than matching encoded strings. The version field is load-bearing — it is what lets P1
add the chain versions and a real projection digest while refusing to honour a P0 token
(ceiling C7).

**Where they apply.** Exact reads carry an `ETag`; a matching `If-None-Match` returns a bodyless
`304`. Batch reads carry validators **beside individual keys**, because one header cannot
represent a batch: any result may be `unchanged`, and the response stays `200` even when all
are. Discovery pages carry no validator and are never conditional — a page is a changing set,
not an exact-key answer.

**Two consequences elsewhere in this spec.** The validator field must be in the SDK models from
the moment the new trait exists, not added afterwards: ~50 call sites across twenty-plus gears
migrate onto that contract, and a later addition buys a second migration. And the client cache
of §8.3 gains the revalidation half of `fr-client-cache` — expiry becomes a batched conditional
`batchGet`, fail-closed on error, instead of dropping the entry.

The toolkit has no `ETag` helper, which is manual work rather than a missing capability:
`OperationBuilder::no_content_response` accepts any status, so `304` is declarable, and
`file-storage` already returns `StatusCode::NOT_MODIFIED` with headers by hand.

### 8.6 Observability of the write path

Admission is asynchronous, so an operator cannot read a decision off the response that triggered
it. What the write path emits is therefore part of the contract, not a by-product:

- **Two spans.** `types_registry.admission.operation` covers one pass over one operation and
  opens before its first read; `types_registry.admission.unit` covers one candidate. Both carry
  `operation_id`, `kind` and `dry_run`, the unit span also `gts_id` and `operation_item_id`.
  Identifiers, selected baselines and dependent counts are **span fields** — per event, not per
  series.
- **Instruments behind a domain port.** `AdmissionMetrics` is a `domain::ports` trait with an
  OpenTelemetry adapter in `infra`, injected at `init`; domain code never names the SDK. The
  rendered names carry a configurable prefix, `_total` on counters and `_seconds` on the duration
  histogram, and they are asserted rather than reviewed.
- **Every label value comes from a closed vocabulary, and no identifier is ever a label.** The
  vocabularies are `status`, `stage`, `reason`, `drift`, `verdict`, `dry_run` and `kind`.
- **Every terminal outcome and every refusal is countable, and the vocabulary is
  compile-enforced.** Acceptance-stage reasons come from an exhaustive match; admission-stage
  reasons come from a `Reason` newtype whose only constructors are named consts, so a refusal
  added later cannot compile without a reason. The one unbounded case — a reason read back off a
  stored `error_payload` — maps to a single `other` label.
- **A series never blends decisions of different kinds.** A dry run performs no write and a
  deletion is not a registration, so `dry_run` and `kind` are labels wherever a counter would
  otherwise merge them. An undecidable compatibility comparison is likewise distinguishable from
  a decided-against one in the metrics, not only in the refusal reason — which is what makes
  §16.12 observable in a deployment.

## 9. Database

`database.sql` is the normative target. P0 creates **10 of its 11 tables**, omitting only
`source_claim` (federation). The exception is `coordination_state`, which arrives in its
own second migration rather than the initial one, because the initial migration is
already applied on every existing installation and would never deliver a new table to it.

- **`coordination_state` exists** — one table for every installation-global
  coordination sequence, named by `state_name`.
- **Only `entity_write_order` is seeded and used in P0**; nothing at runtime seeds or
  addresses any other state.
- **`source_claim` is not created** in P0.
- **The `routing` state row arrives with federation**: its migration seeds `routing`
  together with `source_claim`, and `routing.state_seq` is the routing generation.
- **No standalone `routing_config` table will be created** — in P0 or ever; the
  routing generation lives in the `routing` row above.

**Tenant columns are created and constrained exactly as specified, and never populated
with tenant scope.** Every P0 row carries `ownership_scope = 1`, `owner_tenant_id = NULL`.
This is deliberate: keeping the columns and their CHECK constraints means P1 tenancy
needs no schema migration. `ck_tr_entity_owner` then makes `owning_gear` NOT NULL for
every P0 entity.

| Table | P0 |
|---|---|
| `version_family` | full, global scope only |
| `entity` | full, global scope only |
| `type_schema_revision` | full |
| `instance_revision` | full |
| `type_schema` | full — artifacts materialized (D3) |
| `instance` | full |
| `dependency` | full — written and read (D5) |
| `operation` | full; `plane = 1`, `tenant_id = NULL` |
| `operation_item` | full |
| `coordination_state` | full — `entity_write_order` seeded at 0; `routing` deferred to federation |
| `source_claim` | **not created** |

Migration notes:

- One initial migration, `m2026NNNN_000001_initial.rs`, mapping identity, UUID, binary,
  boolean, timestamp and binary-collation types per backend. Identifier columns:
  `varchar(1024)`, binary collation, ASCII charset where the default is multi-byte.
- `coordination_state` in its own second migration, `m2026NNNN_000002_coordination_state.rs`,
  seeding `entity_write_order` at sequence zero with a migration timestamp; re-running it
  against a database that already has the table and row preserves both.
- Outbox tables come from `outbox_migrations_with_prefix("types_registry_outbox")`,
  not from this migration.
- `routing` is not seeded, because federation has not landed: its migration will seed
  the `routing` row together with `source_claim`.

### Declared ceilings

There is no final compatibility ceiling: `gts-rust` 0.12.0 supplies the tri-state verdict and
the content-model classification, so completed P0 honours `principle-fail-closed` for
compatibility rather than deviating from it (§7). C9 records the implementation window before
that final state and must be struck before the database path is exposed.

C1, C3 and C4 are **struck** — resolved in P0 rather than deferred. The rows are kept
because other documents cite the numbers.

| # | Ceiling | Upgrade path |
|---|---|---|
| C1 | **Struck by D2.** Was: the whole entity set held in process memory, so entity count becomes a memory bound | Resolved in P0 — the store is transient per admission unit and bounded by the unit's dependency closure (§8.2) |
| C2 | `idempotency_scope_hash` digests three constants, so the key namespace is **global**: two unrelated callers reusing one key collide with `409` | Real scope arrives with planes and principals at P1 |
| C3 | **Struck by D11.** Was: the inventory pull model is in-process-only (§8.4) and `owning_gear` a hardcoded constant, which **blocks** out-of-process gears rather than degrading them | Resolved in P0 — `owning_gear` lands on the inventory records (T22) and every gear pushes its own (T23–T25) |
| C4 | **Struck by D2.** Was: startup reads the whole table on the platform boot path, so startup time is linear in entity count | Resolved in P0 — no warm-up read; startup cost is the seed set, not the table (§8.2) |
| C5 | No operation-retention sweep: terminal operations accumulate | The §3.2 sweep, once volume justifies it |
| C6 | **No PDP.** Reads and writes are authenticated but not authorized, deviating from `06`'s *"every sensitive DB access MUST be covered by a PDP decision"*. Entities are `#[secure(unrestricted)]`, so a tenant-scoped query fails closed rather than leaking. **The sharpest edge is the revision path**: §8.1 step 3 asks the registration policy of creations only — correctly, since the policy governs which regions gain members — and nothing takes its place for an edit, so a caller that reaches the submit route can replace the authored content of any entity the registry holds, a platform-seeded `cf.core.*` schema included, in a region the deployment has closed. Bounded in P0 by transport rather than by policy: the mutation routes are internal-only (C8) | Tracked as C6 in the P1 epic #4628 — prerequisite 1 (the deferred identity-to-permission binding), then an owner/principal check before `unit::commit_revision`, and `tenant_col` + `PolicyEnforcer` (§12) |
| C7 | **The validator has no tenant or projection dimensions.** P0's validator digests `resource_version`, `resolution_fingerprint` and a fixed default-projection marker (§8.5); the SDK cache key likewise carries visibility context and projection as constants. Correct while every read is platform-plane and no `$select` exists, and wrong the moment either arrives | The wire form is a **versioned** JSON object, so P1 adds the chain versions and the real projection digest under a new version and refuses to honour a P0 token |
| C8 | **Platform-plane mutations are internal-only.** Every P0 operation is platform-plane (`plane = 1`), but an in-process gear has no inbound platform-identity validator, api-gateway has no platform listener, and `OperationBuilder` cannot mark a route platform-only (§8.4). Registration and deletion therefore keep `exposed = false`; internal and non-mutating calls retain authentication, because `.anonymous()` without a platform identity would be a regression | A platform listener with `X-ToolKit-Internal-Token` / `PlatformIdentity`, a declarative platform-plane route marker, and a platform-principal/PDP decision before mutation dispatch. Only then may mutation routes be exposed. This is toolkit/api-gateway work outside this gear, and ADR-0006/0008 already ask for the listener |
| C9 | **Implementation sequencing only.** T11 makes revisions executable before T14 refreshes reverse impact and T17 compares compatibility. Content revisions **of** minor-bearing Type Schemas remain permanently refused by ADR-0004 — creating one is admissible (§8.1 step 4), editing it is not; during this window every effective `force` also fails closed, and C8 keeps the database mutation path internal | T14 and T17 close the two gaps at Checkpoints 3 and 4, before T24 exposes any consumer. Strike this row when both checkpoints are complete; striking it removes only the temporary `force` refusal, not the ADR-0004 invariant |


Each ceiling gets a `ponytail:`-style source comment naming the bound and the upgrade
path at the point where it bites.

---

## 10. Contracts

### 10.1 New SDK trait

`types-registry-sdk` gains this trait and **loses** `TypesRegistryClient`, which is deleted
once every consumer has moved (D6). During the migration both exist briefly, but the old one
is not a supported surface — it is a step in the cutover, not a deprecation window.

Shape follows DESIGN §3.3, minus tenancy, projections, validators and federation:

```rust
#[async_trait]
pub trait TypesRegistryEntities: Send + Sync {
    /// The one required exact-read primitive. Single and kind-narrowed reads
    /// are provided methods over it, keeping the trait object-safe for
    /// `hub.get::<dyn TypesRegistryEntities>()`.
    async fn batch_get_entities(
        &self,
        request: BatchGet,
    ) -> Result<EntityLookups, CanonicalError>;

    async fn list_entities(&self, query: EntityQuery) -> Result<EntityPage, CanonicalError>;

    async fn register_entities(
        &self,
        key: IdempotencyKey,
        request: RegisterEntities,
    ) -> Result<RegistrationOperation, CanonicalError>;

    async fn delete_entities(
        &self,
        key: IdempotencyKey,
        request: DeleteEntities,
    ) -> Result<RegistrationOperation, CanonicalError>;

    /// Provided: a one-item `delete_entities`, mirroring
    /// `DELETE /entities/{entity_key}`. One deletion model, two spellings.
    async fn delete_entity(
        &self,
        key: IdempotencyKey,
        entity: DeleteItem,
        dry_run: bool,
    ) -> Result<RegistrationOperation, CanonicalError> { /* … */ }

    async fn get_operation(
        &self,
        operation_id: Uuid,
    ) -> Result<RegistrationOperation, CanonicalError>;

    /// Provided: submits, polls to terminality, returns per-identifier outcomes.
    /// This is where the async contract is made ergonomic for startup
    /// reconciliation; it is not a second protocol.
    async fn register_and_await(
        &self,
        key: IdempotencyKey,
        request: RegisterEntities,
        deadline: Duration,
    ) -> Result<RegistrationOperation, CanonicalError> { /* … */ }
}
```

**Convenience read helpers are provided methods** over `batch_get_entities` and
`list_entities`, keeping the trait object-safe while preserving the call shapes consumers
already use: `get_type_schema`, `get_instance`, `get_type_schemas`, `get_instances`, their
`_by_uuid` variants, `list_type_schemas`, `list_instances`. Kind narrowing costs no round
trip, since the kind is the trailing `~` of the identifier. `EntitySnapshot` likewise exposes the
materialized documents as **plain fields** — `content`, `resolved_schema`,
`effective_traits`, `effective_traits_schema` — plus a small `segments` accessor, so a
consumer that previously called the old models' computed methods reads a field instead.
No accessor is needed to reach inside a group, because outside `provenance` there is no
group to reach inside of.

**The old models' client-side `effective_*` methods are deleted, and duplication is the
weakest of three reasons.**

1. **They are wrong for a schema that references outside its derivation chain.**
   `GtsTypeSchema::effective_schema` inlines only the parent's `$ref` and, in its own words,
   leaves *"non-parent `allOf[].$ref` items (mixin references) … as-is"*;
   `effective_properties` / `effective_required` walk `parent` alone. A parent chain is not a
   reference closure, so any `$ref` to a type outside the chain stays unresolved. The server
   resolves through `gts-rust`'s `resolve_schema_refs`, which closes over every resolution-bearing
   reference. An `x-gts-ref` is never inlined and is therefore outside this comparison.
2. **They are a local approximation of GTS semantics**, which
   `constraint-gts-implementation` forbids outright. The code admits it: `effective_traits`
   carries `TODO(#1723): replace with gts-rust's resolve_schema(...).effective_traits once
   that helper is exposed publicly`.
3. **They cannot cross a wire**, because they need the `Arc<GtsTypeSchema>` parent graph —
   the OoP blocker recorded in §8.4.

D3 removes the need entirely: the server materializes `resolved_schema`, `effective_traits`
and `effective_traits_schema` at admission, through `gts-rust`, and a read returns them.

**Consequence for the migration, stated because it will look like a regression.** A
materialized value is not always byte-equal to what the old method returned — it differs
exactly where the approximation was wrong, which is the unresolved-reference and
trait-default cases above. Where they differ, `gts-rust` is right by definition. Twelve call
sites across `account-management`, `resource-group` and `credstore` consume these methods
today; an assertion there that fails after the switch is reporting the old bug, and must be
updated to the materialized value rather than restored.

No security-context argument: P0 has no planes, and adding one now would encode a
signature we know is wrong. When planes land, `ctx` becomes the first parameter of every
method, as DESIGN specifies — a breaking change taken deliberately at that point rather
than faked now.

Models: `EntityKey`, `EntityLookup` (`Found` / `Unchanged` / `NotFound`; no `Failed`
without federation), `EntitySnapshot`, `EntityKind`, `LifecycleStatus`,
`Provenance`, `BatchGet`, `BatchGetItem`,
`RegisterEntities`, `RegisterItem`, `DeleteEntities`, `DeleteItem`,
`RegistrationOperation`, `RegistrationItemResult`,
`OperationStatus`, `CandidateStatus`. Field-for-field the DESIGN §3.3 shapes with the
out-of-scope fields absent — never renamed, so P1 adds rather than rewrites.

Contract-layer purity is enforced by lints DE0101/DE0102: no serde, no utoipa, no HTTP
types in the SDK models.

### 10.2 REST

Business listener, `.authenticated()`, path `/types-registry/v1/...` per DE0801.

| Method | Path | Success |
|---|---|---|
| `POST` | `/types-registry/v1/entities` | `202` + operation; `200` on terminal replay |
| `POST` | `/types-registry/v1/entities:batchDelete` | `202` + operation; `200` on terminal replay |
| `DELETE` | `/types-registry/v1/entities/{entity_key}` | `202` + operation; `200` on terminal replay |
| `POST` | `/types-registry/v1/entities:batchGet` | `200`, one result per requested key |
| `GET` | `/types-registry/v1/entities/{entity_key}` | `200`; `404` when absent |
| `GET` | `/types-registry/v1/entities` | `200` + one content-free page and a cursor |
| `GET` | `/types-registry/v1/operations/{operation_id}` | `200` |

`Idempotency-Key` is required on every mutation, both deletion spellings included. Every `202` carries operation
`Location` and advisory `Retry-After`. Errors are RFC-9457 via
`modkit::api::problem` with `.standard_errors(openapi)`.

`POST /entities` **breaks** (D10): its success shape changes from `200` + per-item results
to `202` + operation, on the same path, with no transitional alias. The route's declared
stability is `unstable`, the change is called out in the changelog, and any REST caller
must move to submit-then-poll. `GET /entities/{gts_id}` keeps its current response shape.

**The break is withdrawn for the T9a–T24 window** (`plan.md` P12). T9 took it early by
repointing the existing v1 routes at the database, which changed `POST /v1/entities`'s
*request* body and so refused its existing callers with `400` rather than handing them a
`202` they could adapt to. Worse, it left
`oagw` and `account-management` writing to the database while still resolving through the
in-memory `TypesRegistryClient`.

**The two versions name their array differently, on purpose.** v1 takes `entities`, v2 takes
`items`, so the T24a promotion refuses a v1 body outright — `400`, missing field `items` —
rather than accepting the field and failing later on elements that changed shape too. `items`
is also what the operation result, the discovery page and `Page<T>` call their array.

So **T9a restores both v1 routes verbatim from `main`** and registers the async surface under
`/types-registry/v2/` instead. Until T24 the two are separate paths over separate stores: no
v1 handler takes `RegistryService`, no v2 handler takes `TypesRegistryService`, and neither
falls back to the other on a miss — a v1-registered identifier is `404` on v2 and the reverse,
asserted rather than tolerated, because a fallback would report an entity as registered when
the admission meant to persist it never ran.

`/v2/` is interim by construction. T24 deletes v1 with the in-memory repository it reads, and
**T24a promotes the async surface onto the v1 paths**, so P0 still ends on one version and the
break above is reinstated there. Route paths come from one constant per version
(`routes::V1` / `routes::V2`, used by the tests too), so the promotion is a constant change
rather than a sweep. The table above describes the **post-T24a** surface, which is the P0
end state.

**`GET /entities` breaks too, and this is the second wire break (D12).** Today it returns
every match in one array, each item carrying its full `content`. DESIGN specifies this route
as *"content-free discovery"* returning *"one page and a cursor"*, and P0 adopts that:

- **A page, not a list.** `limit` defaults to 100 and may not exceed 1000; the response
  carries a cursor when more remains. Ordering is by canonical identifier and deleted
  entities are excluded, which is what makes the cursor a plain keyset — `gts_id` is unique
  and immutable, so a page boundary cannot drift or duplicate. Cursors come from
  `toolkit-odata`, which already encodes them as versioned base64url and refuses an unknown
  version.
- **Content-free by default.** The default field set is identity and metadata; `content`,
  `resolved_schema` and `effective_traits` are not on a discovery page. Arbitrary `$select`
  stays out of P0 (§2), so the default set is the *only* set — which is why it must be the
  right one.

**Callers do not choose fields in P0, and there is one default set per surface, not one per
gear.** A discovery page is content-free; an exact read and `batchGet` return the full
representation, authored content plus the materialized artifacts of D3. Discovery answers
*what exists*, an exact read answers *what is in it*, and DESIGN's *"absent `$select` equals an
explicit default set"* is read per surface.

**A request carrying `$select` is refused, not ignored.** Silently returning a full
representation to a caller that asked for one field is worse than a refusal on three counts:
the caller gets up to 1 MB it did not ask for, it may build on behaviour that P1 will change
under it, and the validator would be computed over a projection the caller does not believe it
has. The refusal is an RFC-9457 problem naming `$select` as unsupported at this version. This
is deliberately unlike the retired cache config keys, which are accepted-and-ignored: a
deployment must not fail to start over a stale setting, while a request must not be answered
with something other than what it asked for (`principle-fail-closed`).
- **No validator on a page** (§8.5): a page is a changing set, not an exact-key answer.

Why this is not deferrable to P1 despite being a break: the current shape is unbounded in
both directions. With effective artifacts materialized (D3) a single response is
*entity count* × up to 1 MB, and the entity count is now every gear's declarations rather
than one gear's. A `limit` alone would not fix it either — without a cursor a caller cannot
reach the rest of the set, so the bound would make the endpoint incomplete instead of large.

**Consequence for the SDK, stated because it changes consumer code.** `list_instances` and
`list_type_schemas` are provided helpers over `list_entities` (§10.1), and their ~87 existing
call sites read payloads from the result. With a content-free page the helpers hydrate through
`batchGet` — one extra round trip per page, absorbed by the client cache (§8.3) on repeat.
The result is complete with respect to the traversal rather than to an instant, which is the
same trade DESIGN accepts for type-filter expansion.

### 10.3 Configuration

Added at `gears.types-registry.config`, defaults per DESIGN §3.8:

```yaml
gears:
  types-registry:
    config:
      allow_compatibility_force: false
      limits:
        authored_document: 256KB
        resolved_document: 1MB
        resolution_closure: 64
        batch_candidates: 100
        activation_write_set: 512      # DESIGN §3.2; the profile is §4
        page_size_default: 100         # `GET /entities`, DESIGN §3.3
        page_size_max: 1000
      registration_policy: {}          # closed by default; global `cf` implicit
      worker:
        operation_timeout: 5m          # accepted, not enforced until T21
        max_revalidation_attempts: 8   # the revalidation loop's bound, §8.1 step 4.3
      local_client:
        cache:
          freshness_window: 30s        # DESIGN §3.3; `0s` disables the window
          store_bound: 64MB            # bytes, not entries — see §8.3
```

The cache keys change shape: `local_client.cache.type_schemas.{capacity,ttl}` and
`.instances.{capacity,ttl}` are replaced by one `freshness_window` and one byte `store_bound`
covering both kinds. The four old keys are accepted-and-ignored for one release with a
warning naming their replacements, so an existing deployment neither fails to start nor
silently keeps a setting that no longer applies.

`ctx.config_or_default()` makes absent config and `config: {}` equivalent to these
defaults. Existing keys (`entity_id_fields`, `schema_id_fields`, `entities`) are retained.

Test fixtures need `registration_policy` entries: `acme`, `test`, `vendor` and `x` are the
common fixture vendors, with `fabrikam`, `contoso`, `globex`, `myvendor`, `acme_corp` and
`nonexistent` in narrower ones. **No production gear is affected** — every declaration outside
`gts.cf.*` is in a test file, an inline `#[cfg(test)]` block or a doc comment, and no
`#[gts_type_schema]` in shipped code names another vendor.

#### Registration policy in P0: one of its two parameters

DESIGN §3.2 gives the policy two independent parameters per GTS Identifier Region — which
vendors may appear in the candidate's last segment, and whether an entity there may be
tenant-owned. **`allowed_vendors` is enforced in P0** (§8.1 acceptance step 3);
**`tenant_ownable` has nothing to decide**, because §9 fixes every P0 row to
`ownership_scope = 1`, `owner_tenant_id = NULL`, so no candidate can be tenant-owned in the
first place.

That is not the same as ignoring the key. Entries merge from the platform release and the
deployment, and a P1-ready deployment will carry `tenant_ownable`; refusing it would fail
startup on a valid configuration, while silently dropping it would let an operator believe a
parameter is enforced. So P0 **parses and validates it, records that it is inert, and refuses
any candidate that asks for tenant ownership** rather than quietly globalizing it — which is
unreachable through the P0 request shapes and is therefore a fail-closed assertion, not a
feature.

Everything else about the policy is P0 behaviour as DESIGN specifies it: closed by default,
the implicit global `cf` allowance, per-parameter resolution (longest literal prefix, exact key
beats any pattern, entries omitting a parameter are skipped, closed default otherwise),
`allowed_vendors` replacing rather than extending a less-specific set, the gate running before
any existence lookup, and revisions and deletions bypassing it so closing a region cannot
freeze existing entities.

The four resolution examples of DESIGN §3.2 are the P0 test matrix, reproduced here because
`tenant_ownable` reads differently under P0's global-only rule:

| Entry key | `allowed_vendors` | `tenant_ownable` | P0 effect |
|---|---|---|---|
| `gts.acme.*` | `[acme]` | `true` | `acme` admitted in its own namespace, including derivations; the ownership half is inert |
| `gts.cf.core.rg.type.v1~*` | `[acme]` | `true` | `acme` may derive from the resource-group type; derivations are global in P0 |
| `gts.cf.core.rg.type.v1~` | `[]` | `false` | the base type itself stays closed — the exact key governs only the base, `…~*` governs the subtree |
| `gts.cf.toolkit.plugins.plugin.v1~*` | `["*"]` | `false` | any vendor may register in the plugin region; still global-only |

The last row is what makes third-party plugin and permission Instances registrable without
opening `gts.cf.*` wholesale, and it is the entry a deployment onboarding another vendor needs
in addition to that vendor's own namespace.

---

## 11. Project structure

```
gears/system/types-registry/
├── docs/p0/{SPEC,plan,todo}.md           ← this spec, its plan and task list
├── types-registry-sdk/src/
│   ├── api.rs                            DELETED once consumers migrate (D6)
│   ├── entities.rs                       NEW  TypesRegistryEntities trait
│   ├── models.rs                         (unchanged)
│   ├── entity_models.rs                  NEW  P0 models per §10.1
│   └── error.rs                          extend: precondition_failed, blocked_by_*
└── types-registry/src/
    ├── gear.rs                           capabilities [system, db, rest, stateful]
    ├── config.rs                         extend per §10.3
    ├── domain/
    │   ├── admission/                    NEW  acceptance, worker, unit commit
    │   ├── compat.rs                      NEW  baseline selection + resolved comparison
    │   ├── dependency.rs                  NEW  extraction + reverse worklist
    │   ├── gts_store.rs                   NEW  build a transient GtsStore from rows (D2)
    │   ├── validator.rs                   NEW  freshness validator digest + wire form (§8.5)
    │   ├── ports.rs                       NEW  persistence ports + the row / input types
    │   ├── service.rs                    rewritten
    │   └── repo.rs                       rewritten: async, DB-backed traits
    ├── infra/
    │   ├── storage/
    │   │   ├── entity/                    NEW  one file per entity, 9 tables (`02`)
    │   │   ├── migrations/                NEW  initial migration + Migrator
    │   │   ├── repo/                      NEW  one file per repository, `runner: &impl DBRunner`,
    │   │   │                                   taking and returning `domain::ports` types
    │   │   ├── store.rs                    NEW  `Repos`: the five ports over the repositories
    │   │   ├── mapper.rs                  NEW  domain row ↔ SDK model `From` impls
    │   │   └── in_memory_repo.rs         DELETED
    │   ├── outbox.rs                      NEW  LeasedMessageHandler + wiring
    │   └── cache/                        retyped onto EntitySnapshot, byte-bounded (§8.3)
    ├── api/rest/                         extend: operations, batchGet, batchDelete, delete-one
    └── ../QUICKSTART.md                   NEW  per `02` (gear has REST endpoints)

gears/system/types-registry/types-registry/tests/
  common/mod.rs                            extend: test_db(), transient-store helpers
  gts_store_test.rs                        NEW  closure containment, load order
  admission_service_test.rs                NEW
  admission_worker_test.rs                 NEW  worker invoked directly, never polled
  dependency_repo_test.rs                  NEW
  compat_test.rs                           NEW
  operation_idempotency_test.rs            NEW
  api_rest_test.rs                         NEW  Router::oneshot
  validator_test.rs                        NEW  digest inputs, 304, batch `unchanged` (§8.5)
  client_cache_test.rs                     NEW  window, `fresh`, revalidation, byte bound (§8.3)
  ready_mode_tests.rs                     DELETED (ready mode is gone)
```

**This tree is indicative, not exhaustive.** It fixes layer placement and names the files whose
location is a decision; the concrete module and test split inside each directory is chosen by
the task list, which is finer-grained (`todo.md` names every file it touches). A file appearing
there but not here is not a deviation — a file in the *wrong layer* is.

Layer placement per `02`. Lint coverage is partial in this repo — DE0201 (DTOs under
`api/rest/`), DE0801 (versioned endpoints), DE0309 (`#[domain_model]`) and DE13xx (no
print macros) are active; DE0101/DE0102 are skipped in `Gears.toml` but the rule stands.

---

## 12. Code style

**Authority is [`docs/toolkit_unified_system/`](../../../../../docs/toolkit_unified_system/),
not `guidelines/DNA/languages/RUST.md`** — the latter is outdated and must not be used as
a reference for this work. Relevant parts: `02_gear_layout_and_sdk_pattern.md`,
`04_rest_operation_builder.md`, `05_errors_rfc9457.md`, `06_authn_authz_secure_orm.md`,
`11_database_patterns.md`, `12_unit_testing.md`.

Workspace clippy is pedantic; `unwrap_used` and `expect_used` are denied.

### Rules this task must follow

| Rule | Source |
|---|---|
| **No plain SQL in handlers, services or repos.** Raw SQL only in migration definitions | `11` core invariants |
| Repository methods take `runner: &impl DBRunner`, **not** `&SecureConn` — the same method then works inside and outside a transaction | `11` |
| Multi-step mutations go through `in_transaction_mapped`, which consumes the `SecureConn` and hands the closure a `&SecureTx` | `11` |
| `#[domain_model]` on **every** non-module-private `struct`/`enum` under `domain/` — enforced by lint DE0309. Strictly module-private types are exempt | `02` |
| SeaORM entities: one file per entity under `infra/storage/entity/`; repositories under `infra/storage/repo/`, one file per repository. `02` writes this as a single `infra/storage/repo.rs`; five repositories in one file read past 1200 lines, so the gear follows `mini-chat`'s `infra/db/repo/` shape and keeps `storage::repo::EntityRepo` as the import path through `repo/mod.rs` re-exports | `02` |
| `#[derive(Scopable)]` with an explicit `#[secure(...)]` on every entity — all four dimensions declared, or `unrestricted` | `06` |
| REST DTOs only in `api/rest/dto.rs`, with serde + utoipa; `From` conversions there or in `mapper.rs`. **Its input is a `domain::ports` row, not a `SeaORM` entity** — entities do not leave `infra/storage/repo/`, which maps them at the edge, so an entity ↔ SDK mapper has nothing to take | `02` |
| Local client adapter in `domain/local_client.rs`, implementing the SDK trait and delegating to the domain service | `02` |
| Gear name kebab-case (`types-registry`), endpoints `/{gear}/v{N}/{resource}` | `02`, `04` |
| A gear with REST endpoints SHOULD ship `QUICKSTART.md` | `02` |

Note on lints: `Gears.toml` currently **skips** `de0101_no_serde_in_contract`,
`de0102_no_toschema_in_contract`, `de0504_client_versioning` and
`de1101_tests_in_separate_files` in this repository. The rules still hold as
conventions — this spec requires them — but do not expect the linter to catch a
violation.

### D5 splits the traversal from the refresh

**The traversal is one scoped recursive CTE.** `toolkit-db`'s ADR-0001 supplies
`SecureSelect::with_ctes` / `SecureCteSelect::recursive_cte`: a `WITH RECURSIVE` built
entirely through `sea-query`, with the scope predicate embedded in both the seed and the
recursive member and no raw SQL anywhere, so `11`'s first invariant is satisfied. One
statement replaces two reads per hop, and the read runs inside the commit transaction while
the candidate's row is locked — where round trips are paid for in contention, not latency.

**The refresh is a loop, and could not be anything else.** Which dependents get *written* is
decided by recomputing each one and comparing `resolution_fingerprint` against the stored
digest — a function of the recomputation, not of the graph. A closure query cannot consult
it, so a CTE can only ever return the *candidate* set that a loop then filters. The bound is
therefore stated over that returned set rather than over the rows the loop ends up writing
(§4): the written set is a subset, so the same number bounds it, and only the walked set is
a number the query itself can refuse on. A candidate whose walk is over the bound is refused
even where the filter would have written fewer — deliberately, because the alternative is
running the whole unbounded refresh to find out.

**The depth cap is the bound, and that pairing is load-bearing.** `recursive_cte` requires a
`max_depth`, and a cap truncates *silently* — a dependent missing from the set keeps stale
artifacts marked current, the one failure this read must not have. Passing the write-set
bound as the cap makes truncation unreachable below the refusal: seed rows carry depth `0`,
so a dependent at shortest distance `d` appears at depth `d - 1`; a hidden dependent would
need a path of at least `bound + 2` edges, every one of whose `bound + 1` intermediate
dependents is nearer and therefore already in the set — which puts the set over the bound,
where the refusal has already fired. A returned set is complete; an incomplete walk is an
error.

**The relation is acyclic, and the walk still deduplicates.** Two edge kinds cannot close a
cycle at all (ADR-0012): derivation strictly shortens the `~`-chain, and nothing references an
Instance. What can is `$ref`, alone or combined with derivation — a base that `$ref`s a schema
derived from it — and admission refuses both over the combined edge set, because an effective
form inlines both kinds and neither cycle has a resolved form. `UNION` is nevertheless required
rather than `UNION ALL`, because a DAG's paths converge and `UNION ALL` would enumerate a
fan-in-heavy graph once per path. The depth cap then does double duty: it bounds the
re-expansion `UNION`-with-depth allows, and it keeps a row that contradicted the acyclicity
invariant from hanging the commit transaction.

Repository-owned traversal, domain-free, no raw SQL:

```rust
// infra/storage/repo/dependency_repo.rs
//
// ponytail: one scoped WITH RECURSIVE over `dependency`, depth-capped at
// limits.activation_write_set (512); measured max fan-out in-repo is 27.
// Over the bound is a refusal, never a truncated set.
// Upgrade path if that bound is hit: the generation/staging protocol in DESIGN §4.
pub async fn reverse_impact(
    runner: &impl DBRunner,
    scope: &AccessScope,
    roots: &[i64],
    bound: usize,
) -> Result<ReverseImpact, ScopeError> {
    let walk = RecursiveCte::<dependency::Entity>::new(
        "reverse_impact",
        Condition::all().add(dependency::Column::ToEntityId.is_in(roots.iter().copied())),
        // the next row's `to_entity_id` points at a walked row's `from_entity_id`
        dependency::Column::ToEntityId,
        dependency::Column::FromEntityId,
        u32::try_from(bound).unwrap_or(u32::MAX),
    );
    let rows = entity::Entity::find()
        .secure()
        .scope_with(scope)
        .with_ctes()
        .recursive_cte(walk)
        .join_cte("reverse_impact", /* entity.id = reverse_impact.from_entity_id */)
        .filter(/* entity.id NOT IN roots */)
        .select_only()
        .column(entity::Column::Id)
        .distinct()
        .limit(u64::try_from(bound + 1).unwrap_or(u64::MAX))
        .all_as::<DependentId>(runner)
        .await?;
    // over the bound -> ReverseImpact::OverBound, which the domain words as a
    // candidate refusal; otherwise the full rows, gts_id-sorted.
}
```

A refusal reached *after* the commit transaction began writing travels as
`WorkerError::RefusedAfterWrite(ItemFailure)` rather than in the `Ok(Err(..))` position
every other candidate refusal uses. The reason is the transaction: the `Ok` position
commits, and this refusal exists to prevent exactly the writes it would commit.
`process_item` unwraps it and records the failure in its own transaction, so the
distinction is invisible past the worker.

Structured logging only: `tracing::info!(gts_id = %id, operation_id = %op, "admitted")`.
No print macros (DE13xx).

### Secure ORM without a PDP

`06` requires every entity to declare its scoping dimensions. P0 entities have no active
tenant dimension, so they carry `#[secure(unrestricted)]` — `06`'s own guidance for
*"truly global tables"* — with a comment recording that P1 switches `entity` and
`version_family` to `tenant_col = "owner_tenant_id"` once tenancy lands. The columns
already exist (§9), so that is a code change, not a migration.

This carries one **named deviation** from a toolkit core invariant. `06` states: *"Every
sensitive DB access MUST be covered by a PDP decision (via `PolicyEnforcer`)."* P0 has no
PDP — deferred by decision, because DESIGN's identity-to-permission binding is itself an
unresolved prerequisite. Consequently P0 reads and writes are authenticated but not
authorized, which is what the current in-memory implementation already does, so this is
not a regression. It is ceiling C6 and it closes when the binding lands.

Because `unrestricted` denies any query whose scope carries tenant IDs, a future
tenant-scoped caller cannot silently read these tables — it fails closed. That is the
property that makes the deviation safe to hold.

---

## 13. Testing strategy

Conventions from `12_unit_testing.md`, which override anything implied elsewhere:

- **No `sleep`, no `timeout`, no `tokio::time::*`, no polling, no retries.** Whole suite
  under 5 s. This has a direct design consequence for D1: **the admission worker must be
  invocable directly** as a function of `(operation_id, runner)`, so tests drive it
  synchronously instead of enqueuing and waiting on the outbox. Outbox *wiring* is
  exercised once, in E2E. Any test that polls an operation is wrong by construction.
- Each test builds its own **SQLite `:memory:`** database and fresh service instances; no
  shared state, parallel-safe. `make test-types-registry-db` on PostgreSQL and MySQL covers the
  backend-specific lock, CAS and range-bound paths.
- Pure logic is `#[test]`, not `#[tokio::test]`.
- **Verify state with direct entity queries**, never only through a service read — a
  scope bug would hide the row.
- Table-driven tests are manual `vec![]` + loop. **Not `rstest`.** Setup helpers are plain
  `async fn` in `tests/common/mod.rs`, not fixtures.
- Naming is `{area}_{scenario}` in snake_case: `admission_update_with_stale_version_fails`,
  `dependency_reverse_impact_reaches_transitive_dependents`.
- Error variants asserted with `assert!(matches!(...), "…got: {err:?}")`.

Target 90%+ coverage. The plain SQLite gear tests plus
`make test-types-registry-db` on PostgreSQL and MySQL are part of done, not a follow-up.

**Unit** — acceptance check ordering, fingerprint computation, family key derivation
(`vM~` / `vM.n~` → one key), shape and contiguity rules, dialect spelling set,
identifier profile refusals, topological order, baseline selection.

**Compatibility semantics** — the tests that pin the 0.12.0 behaviour, from T1 onward:

| Test | Asserts |
|---|---|
| Re-validation sweep over all declared identifiers | every declared identifier still admits under 0.12.0; a failure names the identifier and the finding |
| Generated-schema diff for `#[gts_type_schema]` types | every difference from 0.11.0 output is accounted for, none silent |
| `CompatibilityVerdict::Unknown` from `compare_documents` | candidate fails with a reason **distinct** from `Incompatible`, and nothing is committed |
| Optional property added at a `Closed` level | compatible |
| Same addition at an `Open` level | incompatible — the open level already accepted arbitrary values under that name |
| Same addition at a `Partial` level | `Unknown`, not a guessed verdict |
| Dry Run on an incompatible candidate | reports the offending `ObjectLevel.path`, not just prose |
| Comparison of unresolved documents | never reachable: the code path goes through `compare_documents` only |
| Admitted revision provenance | `gts_spec_version` = `GTS_SPECIFICATION_VERSION`, `gts_impl_version` = crate version |

**Integration, per backend** — these are the tests that would have caught the bugs:

| Test | Asserts |
|---|---|
| Replay of a matching fingerprint | returns the stored operation, `202` non-terminal / `200` terminal, writes nothing new |
| Same key, different fingerprint | `409`, original operation untouched |
| Concurrent acceptance on one key | one winner, loser returns the winner after fingerprint verification |
| Update with stale `expected_resource_version` | terminal item `precondition_failed`, no silent rebase |
| Create when identifier exists | terminal item failure, no revision |
| Concurrent first registration of one family | exactly one succeeds; family ownership is single |
| Minor admitted while `vM~` exists | refused on shape |
| `vM.3~` with `vM.2~` absent | refused on contiguity |
| Deleted predecessor | still counts as compatibility baseline |
| Batch with one failing dependency | dependent `failed` with `blocked_by_dependency`, independent branches commit |
| Circular `$ref`, in one batch or closed by a revision | refused as `invalid_schema`; no cyclic edge is ever stored |
| Revision of a base with N dependents | every dependent's `resolved_schema` and `resolution_fingerprint` refreshed in the same transaction |
| Refresh yielding identical artifacts | fingerprint unchanged, nothing written, `resource_version` not moved |
| Activation set over the bound | candidate fails, no partial refresh committed |
| Duplicate worker invocation on one operation | second invocation is a no-op |
| Worker re-invoked after a rolled-back unit | revalidates from scratch and commits once |
| Dependency moved between evaluation and commit | the guard rolls the commit back; the retry re-resolves against the moved dependency and commits once, so the candidate lands at one new revision |
| Dependent created, or refreshed, after the reverse-impact scan | detected — the first on membership, the second on `resolution_fingerprint` alone, since a refresh moves no `resource_version` |
| `unchanged` re-submission while a dependency moves | still `unchanged`: an outcome that writes nothing is not guarded, so a genuine no-op cannot be turned into a failure |
| Revalidation budget exhausted | item terminal `failed` with reason `revalidation_exhausted`, naming the last drift, and nothing written |
| Two commits in flight at once | impossible: each claims the `entity_write_order` row as its first statement, so the second waits on it until the first ends. Not observable on `SQLite`, which serializes write transactions regardless — **owed as a container-backend test**. What is asserted here is that every commit claims the row at all (`a_creation_claims_the_entity_write_order_row_exactly_once` and its revision / `unchanged` siblings), which is the invariant a future writer can omit |
| Edge committed after a mover's reverse scan | the mover's scan runs after its own claim, so the edge is either already visible to it or belongs to a unit that has not committed — and that unit's own guard sees the mover's revision when it does |
| Dependent's projection moves under an artifact write | the write is a compare-and-swap on the projection state its artifacts were computed against. Unreachable while commits are serialized, since no second commit can move that row; kept as depth for the day a narrower ordering protocol replaces the claim |
| New dependant created while its dependency is being revised | the two serialize on the `entity_write_order` row: whichever commits second sees the first, so either the revision refreshes the new dependant or the dependant revalidates against the new revision. Never two commits and a stale artifact with a matching fingerprint |
| Edge added to a target being deleted concurrently (**T20**) | one of the two refuses; a tombstone never stands over a live registered dependant. The same claim, which deletion makes like every other writer of entity state — the optimistic guard does not cover this either |
| Restart | every entity and its artifacts identical, byte for byte |
| Two pods, commit on A | B's first post-commit read sees it (`nfr-multi-pod-correctness`). Under D2 this holds by construction — B reads the database, and no process-local copy can go stale |
| Second admission after a committed revision | the unit's transient store is rebuilt from the database and sees the new revision without any invalidation step |
| Transient store contents | contains the candidates and their dependency closure, and nothing outside it — a document not reachable from the closure is absent, so an accidental whole-table load fails the test |
| Read inside the freshness window after a direct database change | served from the SDK cache, i.e. stale — asserted deliberately, because this is the trade DESIGN §3.3 sanctions and it must be a decision, not a surprise |
| Same read with `fresh` | bypasses the window and returns the new value |
| Read after a terminal successful registration outcome | the cached entry is gone under **both** its identifier and its UUID key, without waiting for the window |
| `NotFound`, a failed read, a discovery page, an operation resource | never cached, each asserted separately (§8.3) |
| Cache store bound | exceeded by cached bytes, not entry count: one 1 MB document evicts where a thousand small ones do not |
| Expired entry whose content did not change | revalidated to `unchanged` and **kept**, with no full snapshot fetched |
| Two expired keys | one conditional `batchGet`, not two |
| Failed revalidation | error propagates, window is not extended (`principle-fail-closed`) |
| Validator of an unchanged entity | byte-identical across two reads; a revision changes it |
| Validator of a dependent refreshed by a base revision | changes even though its own `resource_version` did not move |
| `If-None-Match` with the current validator | bodyless `304`; with a stale one, `200` and the document |
| `batchGet` with a mix of current and stale validators | `200`, `unchanged` for current keys, snapshots for the rest |
| Instance validator | omits `resolution_fingerprint` and still changes on revision |
| Validator with an unknown version field | rejected, never treated as a match |
| Type Schema whose `$ref` targets a type **outside** its derivation chain | `resolved_schema` closes over that reference too — the case the deleted client-side `effective_schema` left unresolved (§10.1) |
| `effective_traits` of a chain with a trait default at two levels | matches `gts-rust`, not the deleted client-side merge order |
| Request carrying `$select` | refused with an RFC-9457 problem naming the parameter, never answered with the default representation |
| Registration policy, four DESIGN §3.2 entries | each admits and refuses exactly what §10.3's table says, including the exact-key-versus-`~*` split |
| Per-parameter resolution | longest literal prefix wins; an exact key beats any pattern; an entry omitting `allowed_vendors` is skipped so a less-specific entry supplies it; no entry means closed |
| `allowed_vendors` of a more specific entry | replaces, never extends, a less-specific set |
| Region with no entry | first creation refused, naming region **and** parameter |
| Revision or deletion in a closed region | admitted — closing a region must not freeze existing entities |
| Config carrying `tenant_ownable` | parsed and validated, never enforced, and never silently treated as enabling tenant ownership |
| Discovery page | bounded by `limit`, ordered by canonical identifier, deleted entities absent, `content` absent |
| `limit` above `page_size_max` | refused, not silently clamped |
| Cursor traversal over a set larger than one page | every entity appears exactly once across pages |
| Entity admitted mid-traversal | the traversal stays consistent: no duplicate and no skipped predecessor, because the cursor is a keyset over an immutable unique `gts_id` |
| Cursor with an unknown version | rejected rather than reinterpreted |
| `list_instances` helper over a content-free page | hydrates through `batchGet` and returns payloads, so the call shape consumers use is preserved |
| Two pods, concurrent dependency change | commit-time revision-vector mismatch rolls back and retries |
| Dry Run | full check sequence runs, nothing committed, `resource_version` unmoved |
| Delete with live direct dependent | refused; count reported without identities |
| Deleted entity | exact read returns it as deleted; list excludes it |

**E2E** (`testing/e2e/`, pytest) — register → poll → read → re-register unchanged →
delete, over REST, plus the `Idempotency-Key` replay and `409` paths. This is the **only**
place the real outbox dispatch loop is exercised, and the only place polling is allowed,
because it is the only layer where waiting is the behaviour under test rather than an
accident of the harness.

**Compatibility fixture** — pin representative `GTS Identifier → UUID` mappings, per
`constraint-single-installation`, so a `gts-rust` upgrade cannot silently move
references.

---

## 14. Boundaries

**Always**

- Follow [`docs/toolkit_unified_system/`](../../../../../docs/toolkit_unified_system/) for
  code organisation, layering, DB access and test shape. **Ignore
  `guidelines/DNA/languages/RUST.md` — it is outdated.**
- Take GTS semantics from `gts-rust`. A missing behaviour is an upstream change request,
  never a local approximation (`constraint-gts-implementation`).
- Keep repositories on `runner: &impl DBRunner` and the Secure ORM; raw SQL only in
  migration definitions.
- Validate at the admission boundary before touching storage.
- Enforce idempotency uniqueness and `resource_version` CAS in the database, never in
  process memory.
- Run `make ci`, the plain SQLite gear tests, and `make test-types-registry-db` for
  PostgreSQL/MySQL before calling a slice done.
- Sign commits (`git commit -s`), Conventional Commits format.
- Name every deliberate simplification at the point where it bites, with its bound and
  upgrade path.

**Ask first**

- Any change to `database.sql` — it is the normative P1 target, and a P0 deviation from
  it costs a migration later.
- Adding a dependency, or enabling a `preview-` feature beyond the approved
  `toolkit-db/preview-outbox` (D9).
- Deviating from the migration order for `TypesRegistryClient`: the new trait must exist and
  be tested before the first consumer moves (D6).
- Widening scope into anything listed Out in §2.
- Renaming an SDK model field that DESIGN §3.3 names.

**Never**

- Populate `ownership_scope = 2` or a non-null `owner_tenant_id` in P0.
- Create `source_claim` or seed the `routing` coordination state.
- Approximate a compatibility, resolution, or matching semantic locally.
- Take an authoritative admission decision from process-local state without the
  commit-time database recheck (D2, D4).
- Hold a `GtsStore`, an entity map or any other entity-derived state beyond the admission
  unit that built it, or serve a **registry-side** read from such state. Under D2 the only
  correct lifetime is one unit, and the only read source is the database — a process-local
  copy has no invalidation channel in P0 and silently breaks the multi-pod read criterion
  of §13. The SDK client cache of §8.3 is not an exception to this: it is on the other side
  of the contract, bounded by a freshness window DESIGN sanctions, and it is never
  consulted by the registry itself.
- Serve a cached entry for an entity whose mutation the client has already observed, or
  extend a freshness window on a failed read (`principle-fail-closed`).
- Commit a partial effective-artifact refresh.
- Remove or weaken a failing test to make a slice pass.
- Reorder the acceptance checks in §8.1 — the ordering is a disclosure boundary.
- Collapse `CompatibilityVerdict::Unknown` into `Incompatible` — they are separate
  outcomes with separate reasons.
- Write raw SQL in a handler, service or repository.
- Add `sleep`, polling or a retry loop to a unit or integration test (§13). If a test
  needs to wait, the code under test is shaped wrong.
- Introduce `rstest` or fixture-based setup.

---

## 15. Build order

**Superseded by [`plan.md`](./plan.md).** This section originally ordered the work
horizontally — schema, then repositories, then a synchronous admission path a later slice
would have rewritten. `plan.md` P1 replaces it with vertical slices and [`todo.md`](./todo.md)
is the executable task list. The number is kept because other documents cite it.

---

## 16. Success criteria

1. A gear's Type Schemas and Instances survive a process restart with authored content
   and all three effective artifacts byte-identical.
2. `make ci` and the plain gear tests green on SQLite;
   `make test-types-registry-db` green on PostgreSQL and MySQL.
3. Registration returns `202` + operation; polling reaches a terminal state with one
   outcome per candidate GTS identifier.
4. Replaying an `Idempotency-Key` with the same fingerprint writes nothing and returns
   the stored operation; a different fingerprint returns `409`.
5. An update with a stale `expected_resource_version` fails `precondition_failed` — no
   silent rebase.
6. A batch where one dependency fails commits the independent branches and reports
   `blocked_by_dependency` for everything downstream of it.
7. A revision of a base type refreshes every dependent's `resolved_schema` and
   `resolution_fingerprint` in the same transaction; an identical recomputation moves
   no `resource_version`.
8. Two pods against one database: a commit on one is visible to the other's first
   post-commit read, with no process-local entity state anywhere on the read path (D2).
9. Deleting an entity with a live direct registered dependent is refused; a deleted
   entity is still exact-readable as deleted and absent from lists.
10. No tenant-scoped row and no federation table exists in any P0 deployment.
11. Every ceiling in §9 is a comment in the code at the point it binds.
12. The workspace is on `gts-rust` 0.12.0, every declared identifier re-validates, and
    an undecidable compatibility comparison (`CompatibilityVerdict::Unknown`) is rejected
    with its own reason rather than collapsed into `Incompatible`.
13. The SDK client cache is in place on the new models with a byte-bounded store, a
    freshness window, `fresh` bypass, invalidation on an observed terminal outcome and
    batched conditional revalidation — P0 does not ship an uncached read path (§8.3,
    `cpt-cf-types-registry-fr-client-cache`).
14. An exact read carries a validator and honours `If-None-Match` with a bodyless `304`;
    a batch read reports `unchanged` per key while returning `200`; a dependent refreshed
    by a base revision gets a new validator even though its own `resource_version` did not
    move (§8.5).
15. `GET /entities` returns a bounded, content-free page whose cursor traverses the whole
    set exactly once, and no response is unbounded in item count or in bytes (§10.2, D12).
16. Every admission decision is diagnosable from the emitted signals alone (§8.6): each terminal
    outcome and each refusal is counted under a closed vocabulary, an `Unknown` compatibility
    verdict and a forced waiver are each distinguishable in the metrics, and no series blends a
    dry run with a commit or a deletion with a registration.

---

## 17. Open questions

**None remain.** Two answers are load-bearing enough to state rather than merely close:

**O4 — there is no ADR-0015 quarantine preflight scan, and none is needed.** A scan would
establish that no stable schema has a major-0 immediate base or `$ref` target in a registry that
predated the rule, and no such registry exists:
the release that introduces the check is the release that first persists an entity. What
remains is the negative obligation the ADR states — do not enable the rule against a database
populated by a build that had the storage but not the check.

**O2 — `principal_id` is `Uuid::nil()`**, behind the named constant `P0_PRINCIPAL_ID`. A
deterministic UUIDv5 would *look like* a principal a reader could resolve to a subject, and P0
has no platform identity at all (ceiling C8); nil is the honest spelling of "no subject". The
constant carries the greppability, and its docstring carries the nil-write warning. Nothing
downstream depends on the value: C2 makes the `Idempotency-Key` namespace global whichever
constant is digested.

---

## 18. Traceability

- **PRD**: [PRD.md](../PRD.md)
- **DESIGN**: [DESIGN.md](../DESIGN.md)
- **Reference schema**: [database.sql](../database.sql)
- **Code organisation authority**:
  [`docs/toolkit_unified_system/`](../../../../../docs/toolkit_unified_system/) — `02`
  (layout, SDK pattern, `#[domain_model]`), `04` (OperationBuilder), `05` (RFC-9457),
  `06` (Secure ORM, `#[secure]`), `11` (DBRunner, transactions, migrations), `12`
  (test shape). `guidelines/DNA/languages/RUST.md` is **outdated and not used**
- **ADRs honoured in P0**: 0001, 0003, 0004, 0005, 0006, 0008, 0012, 0014, 0015
- **ADRs out of P0 scope**: 0002, 0007, 0009, 0010, 0011, 0013
- **`gts-rust`**: 0.12.0 from crates.io, declaring `GTS_SPECIFICATION_VERSION = "0.13"`
- **Prerequisites closed here**: activation write set (§4), `sea-query` recursive CTE
  (D5 — verified available and *used*, on all three backends), GTS capabilities 1–7 of
  DESIGN §4 via the 0.12.0 upgrade (§7)
- **Prerequisites still open**: worker liveness bounds
  (§10.3 `worker.*` proposes values), benchmark profile. GTS capability 8 (pattern
  containment) is deferred with federation
- **`preview-outbox` reliance**: approved (D9), matching `ledger`, `file-storage`,
  `chat-engine`
