# Types Registry P0 — Task List

Plan: [`plan.md`](./plan.md) · Spec: [`SPEC.md`](./SPEC.md)

Standing bar for every task, on top of its own acceptance criteria: `make fmt`, `make clippy` and
gear tests green, no regression in other gears, behaviour verified at runtime, docs updated.
**The full `make ci` is a checkpoint gate, not a per-task one** — it ends in `dylint` and pulls in
four container-backed targets, which is `plan.md` P13's whole point. Naming `make ci` per task is
what made T1–T9 record it as *partial* every time.
Code organisation follows `docs/toolkit_unified_system/` — **not** `guidelines/DNA/languages/RUST.md`.

`TR/` abbreviates `gears/system/types-registry/types-registry/`, `TR-SDK/` abbreviates
`gears/system/types-registry/types-registry-sdk/`. Other paths are from the repository root.

**No new ADRs.** The no-PDP deviation is recorded in SPEC §9 (ceiling C6) and §12; the two wire
breaks in SPEC §10.2 — `POST /entities` (D10) and the paged content-free `GET /entities` (D12).

---

## Commands

Every task below that says **gear tests** means both halves:

```bash
# SQLite — the default backend; every unit and integration test runs here
cargo nextest run -p cf-gears-types-registry

# PostgreSQL + MySQL, one testcontainers container per test (Docker required)
make test-types-registry-db
```

**Per-task versus checkpoint.** Per task: `make fmt`, `make clippy`, gear tests. At the
checkpoint: the full `make ci`, which adds `dylint` (whole-workspace build — `plan.md` P13),
`deny`, `lychee`, `gts-docs` and the container targets.

---

## Phase 0 — Upgrade

### - [x] T1: Upgrade to `gts-rust` 0.12.0 via `[patch.crates-io]`

**Description:** Point the workspace at the local `gts-rust` checkout so the tri-state
compatibility verdict, `ContentModel` classification and `compare_documents` become
available, then prove the corrected semantics break nothing already declared. This task's
failure mode is other gears, which is why it runs first and alone.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] `gts`, `gts-id` and `gts-macros` are all three at **`0.12.0`** from crates.io — they move together, and `gts-dylint` / `gts-macros-cli` must not lag (SPEC §7, D8)
- [x] Every declared GTS identifier still admits; any that does not is fixed or explicitly waived in writing. **The figure "202" is not reproducible** and is replaced by three measured populations: 118/118 entities admitted at runtime under the e2e feature set (34 Type Schemas + 84 Instances), 797 doc/JSON files validated by the 0.12.0 validator, and every macro literal compiled by the workspace test run
- [x] Every difference in `#[gts_type_schema]`-generated schema documents versus 0.11.0 is enumerated and accounted for — none silent. 9 of 118 documents differ, all by one change (a doc-commented GTS-identifier field's subschema preserved under `allOf` instead of having its `description` overwritten); shown semantically inert, including `x-gts-ref`, which `XGtsRefValidator` still enforces by recursing through `allOf`

**Verification:**
- [x] `make gts-docs` — 797 files, 0 errors
- [x] `cargo test --workspace` — 10213 passed, 368 skipped, 0 failures; baseline was 10206 passed + 368 skipped, and the difference is exactly the 6 new capability tests
- [x] Manual: generated schema documents captured before and after by booting the example server and dumping `GET /cf/types-registry/v1/entities`, then diffed field by field

**Added:** `TR/tests/gts_012_semantics_tests.rs`, 6 tests pinning the tri-state verdict,
`ContentModel` including `Partial`, `GtsStore::compare_documents` and the provenance
versions. Under 0.11.0 the file does not compile — none of those symbols exists in the
published crate — so it is a genuine RED→GREEN.

**Dependencies:** None
**Files likely touched:** `Cargo.toml`, plus test fixtures the corrected semantics reject
**Scope:** M (mechanical change, large verification surface)

---

### Checkpoint 0
- [ ] `make ci` green — **partial, see T1**: everything that does not need Docker is green
- [x] `make dylint` — full workspace, once for the phase (P13). Satisfied by the workspace-wide run recorded at Checkpoint 1: Phase 0 is one task and that run included its changes
- [x] Every declared GTS identifier admits under 0.12.0
- [x] Generated-schema diff reviewed and accounted for — one class of change in 9 of 118 documents
- [ ] **Human review before any registry code is written**

---

## Phase 1 — One global entity of each kind, persisted, async, end to end

Exercised by fixtures and REST only. Consumers stay on the in-memory path until T24, so
nothing in this phase can regress another gear.

### - [x] T2: Migration for the 9 tables

**Description:** One initial SeaORM migration creating the P0 subset of `database.sql`:
`version_family`, `entity`, `type_schema_revision`, `instance_revision`, `type_schema`,
`instance`, `dependency`, `operation`, `operation_item`. Tenant columns and their CHECK
constraints are created exactly as specified and never populated with tenant scope, so P1
tenancy needs no migration. The database scope, stated once: `coordination_state` exists
(the second migration), only `entity_write_order` is seeded and used in P0,
`source_claim` is not created, the `routing` state row arrives with federation, and a
standalone `routing_config` table is never created in any phase.

**Acceptance criteria:**
- [x] All 9 tables, their PKs, FKs, UNIQUE and CHECK constraints, and the 4 indexes from `database.sql` are created — `idx_tr_operation_status`, `idx_tr_entity_family`, `idx_tr_entity_visibility`, `idx_tr_dependency_to`. Conformance was **measured**, not argued: the Postgres list reproduced `database.sql`'s P0 constraint set 48 for 48 and all three dialects declared the same columns in the same order. That was a one-time measurement — the standing guard behind it was removed after Checkpoint 1
- [x] Identifier columns are `varchar(1024)` with binary collation and ASCII charset where the backend default is multi-byte — `family_key`, `entity.gts_id`, `operation_item.gts_id`: `varchar(1024) COLLATE "C"` on Postgres, `VARCHAR(1024) CHARACTER SET ascii COLLATE ascii_bin` on `MySQL`, `TEXT COLLATE BINARY` on `SQLite` (its default, stated so a later `COLLATE NOCASE` cannot creep in)
- [x] Enumerations stored as smallint with CHECKs enumerating allowed values. Two forms, both present and both tested: an explicit `IN` list for `kind`, `status`, `entity_kind` and `dependency.kind`; and the branch CHECK for `ownership_scope`, `plane` and `lifecycle_status`, where no branch matches a third value — `ck_tr_version_family_owner`, `ck_tr_entity_owner`, `ck_tr_operation_plane` and `ck_tr_entity_lifecycle` already close those domains, so adding an `IN` list would have been a constraint `database.sql` does not have
- [x] `DatabaseCapability::migrations()` returns the Migrator; outbox tables come from `outbox_migrations_with_prefix("types_registry_outbox")`, not from this migration. Both halves are tested: one test asserts the initial migration alone creates **no** outbox table, another applies the gear capability's full set and asserts the 9 managed tables and the prefixed outbox tables all exist
- [x] Raw SQL appears only in migration infrastructure (`11_database_patterns.md` invariant) — the three statement lists plus the drop list live in `m20260817_000001_initial.rs`, and `m20260904_000002_coordination_state.rs` carries its own three lists plus drop list the same way; no gear code gained SQL

**Verification:**
- [x] Migration up and down on **SQLite** — `tests/migration_test.rs`, 41 tests, in-memory `SQLite` with `PRAGMA foreign_keys = ON`, including a full `up` → `down` → `up` roundtrip
- [x] Migration up and down on **PostgreSQL and MySQL**. `tests/migration_backends_test.rs` (behind `--features integration`, per the `account-management` precedent) brings up a Postgres and a `MySQL` 8.1 container, applies the migration, re-checks `ck_tr_entity_owner` / `ck_tr_operation_item_state` / FK `RESTRICT` against the real engines, and rolls back. Written here, first **run** at Checkpoint 1 once Docker was up — which is where it found the uuid-binding defect
- [x] Test: each CHECK constraint rejects the shape it names — all 19 CHECKs of the Postgres set, and the 3 boolean-domain CHECKs the `SQLite` / `MySQL` lowering adds, have a test. Two needed isolation work rather than a hedge: `ck_tr_operation_item_dry_run_bool` fires on a row that also breaks the composite FK, so its test runs against a database with `PRAGMA foreign_keys = OFF` and a control insert proving the switch-off took; `ck_tr_instance_revision_no` needs the whole entity → item → type-schema-revision chain seeded first. Three (`ck_tr_operation_item_kind`, `..._status`, and the third-plane / third-scope / third-kind cases) are shown to reject the *shape* rather than attributed to one named constraint, because a second CHECK matches the same row — stated in the test comments, not claimed away
- [x] Test: `ck_tr_operation_item_state` rejects a `succeeded` non-dry-run registration item with no `result_revision_no` — plus the positive case, the dry-run success that wrongly allocated a resource version, and `unchanged` on a first admission
- [x] Manual, at runtime: booted `cf-gears-example-server` with a temporary config binding a `SQLite` database to types-registry. Both migrations applied (`applied=2 skipped=0`) and the file holds the 9 managed tables, the 4 indexes and the 7 prefixed outbox tables. The boot then fails in `oagw` post-init (`Failed to resolve root tenant: no plugin available`) — reproduced identically with the **pristine** `config/quickstart.yaml`, so it pre-dates this task

**Dependencies:** T1
**Files touched:**
- `TR/src/infra/storage/migrations/mod.rs` — NEW, Migrator
- `TR/src/infra/storage/migrations/m20260817_000001_initial.rs` — NEW, three statement lists + drop list
- `TR/src/infra/storage/migrations/m20260817_000001_initial_tests.rs` — NEW, 12 in-source tests
- `TR/tests/migration_test.rs` — NEW, 41 `SQLite` schema tests
- `TR/tests/migration_backends_test.rs` — NEW, 2 container-backed tests behind `integration`
- `TR/src/gear.rs` — capabilities `[system, db, rest]`, `DatabaseCapability`
- `TR/src/infra/storage/mod.rs`, `TR/src/infra/mod.rs`, `TR/src/lib.rs` — re-export `Migrator`
- `TR/Cargo.toml` — `sea-orm`, `sea-orm-migration`, `toolkit-db`, `toolkit` feature
  `preview-outbox`, `integration` feature, `testcontainers` dev-deps
**Scope:** M — one long DDL file; deliberately not split, because splitting it orders FKs across tasks

---

### - [x] T3: SeaORM entities for the core six

**Description:** Entity structs for `version_family`, `entity`, `type_schema_revision`,
`type_schema`, `operation`, `operation_item`, each with `#[derive(Scopable)]` and
`#[secure(unrestricted)]` plus a comment recording the P1 switch to
`tenant_col = "owner_tenant_id"`.

**Acceptance criteria:**
- [x] One file per entity under `TR/src/infra/storage/entity/` (`02_gear_layout_and_sdk_pattern.md`). `entity/entity.rs` is module inception and deliberately so — these files are a DDL mirror, so each is named after its table; renaming would put the mirror out of step with the schema it tracks. One targeted `#[allow(clippy::module_inception)]` on the `mod` declaration, with that reason
- [x] Every entity declares its security dimensions; none omits the attribute. The `Scopable` derive already refuses to compile without an explicit decision per dimension, so the criterion is met by construction — but *which* decision was made is what a future edit could change silently, so it is also a test: `every_core_entity_is_declared_unrestricted_while_ceiling_c6_stands` asserts `IS_UNRESTRICTED` and all four dimension columns `None` for all six
- [x] `ponytail:`-style comment on the `#[secure(unrestricted)]` attributes records ceiling C6 and its upgrade path. Three variants, because the tables differ in what they *could* be scoped by: `version_family` / `entity` own `owner_tenant_id` and name the switch to `tenant_col` directly; `operation` owns `tenant_id` and additionally records ceiling C8, since the plane is expressed by that column rather than enforced by the transport; `operation_item`, `type_schema_revision` and `type_schema` own **no** owner column at all — ownership is the parent entity's — so their note says `unrestricted` is the only honest marker today and leaves the copy-versus-scoped-read choice with the `PolicyEnforcer` work
- [x] Enumeration columns map to typed Rust enums with explicit smallint conversion, storage-only. Seven `DeriveActiveEnum` vocabularies over `rs_type = "i16", db_type = "SmallInteger"` with explicit `num_value`. **No `Serialize` / `Deserialize` / `ToSchema` is derived on any of them**, so the integers cannot reach the wire by accident — that is the mechanism behind "storage-only", not just a convention

**Verification:**
- [x] `cargo test -p cf-gears-types-registry` — 153 lib + 41 migration + 7 entity + the pre-existing suites, 0 failures
- [x] Test: enum ↔ smallint round-trip for every vocabulary, asserting the exact numbers in `database.sql` — 10 tests. Every case is written out literally rather than derived from variant order, because deriving it from the order would restate the bug it guards. One test pins the *count* per vocabulary, so a variant added without a case here fails; one asserts every out-of-vocabulary integer fails to parse, so a row written by a future version is a clean read error rather than a silent misinterpretation; and one exists purely to stop a future reader "unifying" `OperationStatus::Completed = 3` with `OperationItemStatus::Succeeded = 3`, which `database.sql` says is coincidence and MUST NOT become a contract
- [x] **Known open gap, by decision.** `every_core_entity_declares_exactly_the_columns_database_sql_defines` was written, mutation-checked by deleting `operation.request_fingerprint`, then removed after Checkpoint 1 along with the shared `database.sql` parser. The gap it closed is therefore open: the round-trip tests in `tests/entity_test.rs` prove the columns an entity *names* exist, and cannot notice one it **omits**, because `SeaORM` simply never selects it and the read succeeds. `every_core_entity_binds_to_its_table_in_the_migration` survives in `entity/columns_tests.rs` — it needs no DDL parser
- [x] Added: `tests/entity_test.rs`, 7 tests writing and reading every core entity against the real migrated schema. The timestamp round-trip is the specific risk worth covering — `timestamptz` lowers to `TEXT` on `SQLite`, so an `OffsetDateTime` through `SeaORM` is a real conversion — and two tests write shapes `ck_tr_entity_lifecycle` and `ck_tr_operation_item_state` constrain, so a successful write is itself evidence the mapping agrees with the DDL

**Three of the tables have no entity yet, deliberately.** `instance` and
`instance_revision` arrive with Registered Instances (T10), `dependency` with edge extraction
(T13). An entity with no reader is code the compiler cannot check against the DDL, which is
exactly the drift these mirrors exist to prevent.

**No relations are declared** on any entity. `entity.family_id`,
`type_schema_revision.operation_item_id` and the composite current-state pointers are real
foreign keys, but nothing joins across them yet — the T4 repositories read the family by key
under its own lock. An unused `has_many` would be code with no reader.

**Dependencies:** T2
**Files touched:**
- `TR/src/infra/storage/entity/mod.rs` — NEW
- `TR/src/infra/storage/entity/enums.rs` — NEW, 7 storage vocabularies
- `TR/src/infra/storage/entity/enums_tests.rs` — NEW, 10 tests
- `TR/src/infra/storage/entity/{version_family,entity,type_schema_revision,type_schema,operation,operation_item}.rs` — NEW
- `TR/src/infra/storage/entity/columns_tests.rs` — NEW, 2 conformance tests
- `TR/src/infra/storage/normative_schema.rs` — NEW, shared `database.sql` reader + 3 self-tests (**deleted after Checkpoint 1**)
- `TR/src/infra/storage/migrations/m20260817_000001_initial_tests.rs` — drift tests rewired onto the shared parser
- `TR/src/infra/storage/mod.rs` — declare `entity`, `normative_schema` (the latter since removed)
- `TR/tests/entity_test.rs` — NEW, 7 DB-backed tests
- `TR/Cargo.toml` — `toolkit-db-macros`, `time` (+ `macros` for `datetime!` in tests)
**Scope:** M — 7 files, each mechanical; grouped because they are one DDL mirror

---

### - [x] T4: Repositories on `DBRunner`

**Description:** Repository methods for the core six, taking `runner: &impl DBRunner` and
`scope: &AccessScope` so the same method works inside and outside a transaction. Keyed
reads, insert-if-absent for `version_family`, compare-and-swap on
`entity.resource_version`, and canonical-order locking helpers.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] Every method takes `runner: &impl DBRunner`, never `&SecureConn` — and one test passes a transaction to the same methods, so a signature that quietly narrowed to `DbConn` would fail
- [x] No raw SQL; all queries go through the typed builder
- [x] Compare-and-swap on `resource_version` is a single statement whose affected-row count is the success signal. A stale precondition is `Ok(false)`, not an error — an ordinary concurrent-writer outcome the caller turns into `412`
- [x] Family create-then-read works on all three backends. **The "locked read" half of the original criterion is not achievable:** `DBRunner` hides the raw executor and the secure builder exposes no lock clause, so a repository cannot take `SELECT … FOR UPDATE`. `create_or_get` makes `uq_tr_version_family_key` the serialization point instead — the loser's conflict is **absorbed** (`ON CONFLICT DO NOTHING`), not raised, and then re-read. Serializing the *validation* window needs the toolkit advisory lock on the `Db` handle, which is service-layer (T12); `lock_order` is the ordering half. Now **run** on both container backends, inside a transaction as well as on a pooled connection — see the correction below
- [x] Read primitives for the database read path (SPEC D2, §8.2): a keyed exact read, and a list read that prefilters in SQL on the stored columns and then applies `GtsId::matches_pattern` in Rust. **GTS identifier matching is never translated into SQL** — the prefix range is deliberately *wider* than the pattern (it drops the final segment, because minor-version flexibility would otherwise make the range exclude a real match), and `GtsId::matches_pattern` alone decides
- [x] The list read is a **keyset page**: `gts_id > :after ORDER BY gts_id LIMIT :n`, excluding deleted rows, so a page boundary cannot drift or duplicate (D12). It reports whether more remains — `has_more` may over-report, which is the safe direction — and never loads the whole match set: SQL is asked for bounded batches, each row is tested as it arrives, and the scan stops on the page limit or the scan budget
- [x] A dependency-closure read: given candidate identifiers, return them plus the transitive closure of what they consume, walking `dependency` edges (D5), `gts_id`-sorted. Candidates with no entity row are **reported** in `missing_roots` rather than failing the read, because a first admission's own candidate is exactly that case

**Verification:**
- [x] Gear tests (see [Commands](#commands)) — 161 lib + 149 integration tests, of which 18 are `repo_test.rs` and 7 are `repo_tests.rs`
- [x] Test: concurrent `version_family` creation yields exactly one row — 8 tasks against a file-backed pool; `SQLITE_BUSY` is retried rather than pretended away, and the assertion is on the end state
- [x] Test: CAS with a stale version affects zero rows and is reported as such
- [x] Test: list read with a wildcard pattern returns exactly what `GtsId::matches_pattern` accepts, including a case the SQL prefilter admits but the pattern rejects. **Two fixture identifiers had to be fixed first:** `…v1~x.a.type.v1~` does not parse (a chain segment is a full `vendor.package.namespace.type.vMAJOR`), so `matches_pattern` rejected it and the test had been passing for the wrong reason. The `v1~*` expectation was also wrong — a bare segment is an implicit derived-type envelope (GTS spec §3.6), so `…v1~` and `…v1~*` accept the same set, base included
- [x] Test: keyset paging over a set larger than one page yields every row exactly once, and a row inserted mid-traversal neither duplicates an earlier row nor hides a later one
- [x] Test: closure read over a chain returns the whole chain and nothing outside it — plus termination on a row that contradicts acyclicity. The relation is a DAG (ADR-0012), so the `seen` set is what keeps the walk linear in entities rather than in converging paths; termination on a contradicting row is defence in depth, retitled at T14 when the invariant gained a test of its own
- [x] `cargo test --workspace` (excluding the two macro crates, as `make test-no-macros` does) — passes, so no regression in any other gear
- [x] PostgreSQL / MySQL repository primitives — **run, and they found two defects.** `tests/repo_backends_test.rs` covers the properties `SQLite` cannot demonstrate: the unique-conflict handling, the keyset cursor's binary collation, and now the same two races **inside a transaction**, which is the only shape production uses. Both container suites pass; see the correction below. Run: `cargo test -p cf-gears-types-registry --features integration --test repo_backends_test`

**Added beyond the acceptance criteria:**
- **Boundedness is tested, not asserted.** 2100 rows inside the prefix range that the pattern rejects, with the single match sorted last: a read that materialised the range would return it on the first page, a bounded scan cannot. Mutation-checked by raising `SCAN_BUDGET`
- **The SQL batch adapts.** With a pattern, 256 rows per round trip so a sparse match set does not cost one trip per match; without one, the page's own remainder, because nothing can be rejected and reading ahead is waste. Still capped at 256 either way — the remainder is caller-supplied, and one round trip's memory must not be
- **`replace_outgoing` treats its edge list as a set** — `(from, kind, to)` is the primary key, so a schema that `$ref`s one base twice would otherwise be a PK violation mid-admission. Mutation-checked
- **A second deletion is proved to be a no-op** — `mark_deleted` requires `Active`, so a repeated call reports failure and leaves `deleted_at` where it was; the read-back also pins the `LifecycleStatus` enum lowering through `Expr::value`, which no `ActiveModel` covers
- **Two pre-existing T3 clippy failures fixed** (`make clippy` runs `--all-features`, so both would have failed CI): `entity_test.rs`'s bare-connection reads, allowed at file scope with the reason that the file tests the entity rather than the scope; and an unbackticked `PostgreSQL` in `migration_backends_test.rs`'s module header

**A correction, found by running the container suites.** `create_or_get` caught the loser's
unique violation and re-read *through the same runner* — which in production is always `&DbTx`,
and the two backends disagree about what a raised violation does to one. **`PostgreSQL` aborts
it**, so the recovery could never have worked on the backend it was written for; the `SQLite` test
passed only because it uses a pooled connection. **`MySQL` hides the winner** — with the violation
absorbed instead, the loser's re-read returned nothing under `InnoDB`'s default `REPEATABLE READ`,
the row having been committed after its snapshot opened. Fixed at the layer that owns each:
`repo::conflict_do_nothing` absorbs the conflict so nothing is ever aborted, and
`ports::commit_write` asks the commit transaction for `READ COMMITTED` so a recheck sees what
another admission just committed — the mirror image of `snapshot_read`. `insert_entity` got the
same treatment and now returns `Option`, where `None` is a lost race the worker records as
`already_exists` instead of the `500` a raised violation produced. Each half is pinned by an
in-transaction backends test, and each was confirmed to fail with its fix reverted.

**Dependencies:** T3
**Files touched:**
- `TR/src/infra/storage/repo.rs` — NEW, three repositories (**split into `repo/` — one file per repository — after Checkpoint 1**, see the Phase 2 preamble)
- `TR/src/infra/storage/repo_tests.rs` — NEW, 7 in-source tests for the SQL prefilter
- `TR/src/infra/storage/mod.rs` — `pub mod repo`
- `TR/tests/repo_test.rs` — NEW, 17 `SQLite` tests against the migrated schema
- `TR/tests/repo_backends_test.rs` — NEW, 2 container-backed tests behind `integration`
- `TR/tests/common/mod.rs` — `SQLite`/DSN provider harness, `allow_all()` scope
- `TR/tests/entity_test.rs`, `TR/tests/migration_backends_test.rs` — the two T3 clippy fixes
**Scope:** M

---

### - [x] T5: Transient `gts-rust` store built from database rows

**Description:** One function that builds a `GtsStore` from a set of database rows, for use by
a single admission unit and dropped with it (SPEC D2, §8.2; `plan.md` P6). It takes the
candidates plus the transitive closure of what they consume — obtained from the `dependency`
table — reads them `gts_id`-sorted so a derived schema never loads before its base, and
returns an owned store. Nothing is cached, published or shared: **no `ArcSwap`, no snapshot,
no process-lifetime store.** Reads are served from the database, not from here. The old
in-memory repository keeps serving the old trait until T24 and is not touched.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] Signature takes a row set (or a closure query) and returns an owned `GtsStore`; it stores nothing in `self` and registers nothing globally. Two layers: `build_store(Vec<UnitDocument>)` is pure — no database, no clock, no global state — and `load_unit_store(runner, scope, candidates)` reads the closure and delegates to it. Both are free functions; there is no `self` to store anything in. The store is returned inside `UnitStore`, which owns it outright: no `Arc`, no lock, and no way to clone it out
- [x] Load order is `gts_id`-sorted, so a derived schema never loads before its base — **but the stated mechanism is wrong and is corrected here.** `register_schema` inserts into a map and validates nothing; it is `validate_schema` / `resolve_schema_refs` that walk `chain_ids()` and fail on an absent base. Registering a derived schema first therefore does **not** fail in `gts` 0.12.0. The sort is kept because it makes the store complete before anything is asked of it, which turns a latent ordering bug into an impossible one — not because registration needs it
- [x] The store is built from the unit's dependency closure, not from the whole `entity` table — a whole-table load must fail the closure test below. **Mutation-checked rather than argued:** substituting `EntityRepo::list_page(None, 1000)` for the closure read fails exactly two tests and leaves the other eight green
- [x] No lock of any kind: the store is owned by one caller, so `GtsOps` not being `Sync` is irrelevant rather than worked around. `GtsStore` is `Send + !Sync` because `GtsReader` has no `Sync` supertrait — the exact reason the old in-memory repository holds a `Mutex<GtsOps>`. Nothing here holds one, and the store still crosses an `.await`, which is all an admission unit needs
- [x] Closure lookup uses the `dependency` edges (D5), and a candidate with no dependencies yields a store containing only the candidate — the first-admission case, whose candidate is reported in `missing_candidates` (T4's `missing_roots`) rather than failing the read

**Verification:**
- [x] Gear tests (see [Commands](#commands)): 170 lib + 159 integration tests, of which 9 are the in-source builder tests and 10 are `gts_store_test.rs`
- [x] Test: store built from a chained fixture resolves the derived schema's base — a candidate whose document `$ref`s `gts://<base>`, with the base supplied by the closure; the resolved document carries no `$ref` and does carry the base's own property. Its `$id` stays a `gts://` URI, because `resolve_schema_refs` inlines references and does not rewrite identity
- [x] Test: closure containment — a document not reachable from the candidate's dependency closure is **absent** from the store. The stranger is committed, active and in the same family, so only reachability distinguishes it
- [x] Test: rows are consumed in `gts_id` order given deliberately shuffled input — asserted through `UnitStore::load_order()`, because a `HashMap` keeps no trace of insertion order and the criterion is otherwise untestable
- [x] Test: two sequential builds after a committed revision each observe the new revision, with no invalidation step between them — the second revision is committed directly, so the builder learns of it only by re-reading, and the old content is asserted absent as well as the new one present
- [x] Added: the candidate overlay beats the committed document under the same identifier (D19's in-batch rule, and what T11 needs); a tombstoned base still loads, because it stays the compatibility baseline until purge; a document-less entity and a stored non-JSON document are each named rather than surfacing later as `UnresolvedRefs`; and the same builder runs inside a transaction, so T8 can build its store inside the transaction it commits in
- [x] Added: a dialect-less document is refused by name, and an empty `$schema` separately. Without `$schema`, `register_schema` registers the document as an **Instance** — `GtsEntity::new` overwrites the `is_schema: true` it was passed — and every `$ref` at it then stays unresolved with no error anywhere
- [ ] PostgreSQL / MySQL `current_documents` — **written, not run: Docker daemon down.** One case in `tests/repo_backends_test.rs` for the two properties `SQLite` cannot show: the exact-pair disjunction binds two parameters per entity, and `raw_schema` is `text` / `LONGTEXT`, so a document past any `varchar` bound is evidence about the column type. Run: `cargo test -p cf-gears-types-registry --features integration --test repo_backends_test`

**Dependencies:** T4
**Files touched:**
- `TR/src/domain/gts_store.rs` — NEW, pure builder + DB loader + `UnitStore` / `UnitDocument` / `StoreBuildError`
- `TR/src/domain/gts_store_tests.rs` — NEW, 9 in-source tests, no database
- `TR/src/domain/mod.rs` — declare `gts_store`
- `TR/src/infra/storage/repo.rs` — NEW `TypeSchemaRepo::current_documents`, `CurrentDocument`, `PAIR_CHUNK`
- `TR/tests/gts_store_test.rs` — NEW, 10 `SQLite` tests
- `TR/tests/common/mod.rs` — shared managed-state fixtures (operation → item → revision → pointer)
- `TR/tests/repo_backends_test.rs` — one container-backed case for `current_documents`
**Scope:** S — as estimated for the builder; the content read is the unplanned half

---

### - [x] T6: Typed configuration

**Description:** Extend `TypesRegistryConfig` with `allow_compatibility_force`, `limits.*`
(including P0's `activation_write_set`), `registration_policy` and `worker.*`, keeping the
existing keys. The `local_client.cache.*` keys stay live — the cache is kept (SPEC §8.3) — and
their reshaping into `freshness_window` / `store_bound` belongs to T30, not here.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] Absent config and `config: {}` both yield the SPEC §10.3 defaults via `ctx.config_or_default()` — asserted as the *same value* rather than each checked against the table, plus a second test pinning every default against §10.3 value for value
- [x] `registration_policy` keys are validated at startup; an invalid GTS pattern fails startup rather than being skipped. `TypesRegistryConfig::validate()` returns the **compiled** `RegistrationPolicy` rather than `()`, so the boot path and the acceptance path consult one compilation instead of two that could disagree
- [x] `tenant_ownable` is **parsed and validated but inert** (SPEC §10.3). `RegistrationPolicy::tenant_ownable` resolves it by the same rules as the vendor set and is asserted to return the configured value — which is what makes it inert rather than dropped — while `admits` refuses any candidate asking for tenant ownership whatever the entry says. §9 makes that request shape unreachable, so it is a fail-closed assertion, not a feature
- [x] Per-parameter resolution implemented: longest literal prefix wins, an exact key beats any pattern, entries omitting a parameter are skipped, closed default otherwise. Specificity is `(is_exact, literal_prefix_len)` — the exact-beats-pattern rule is a separate tuple element rather than left to fall out of prefix lengths, because DESIGN states it separately
- [x] Global `cf` vendor is implicitly admitted; nothing else is. Keyed on the **last** segment's vendor, so a `cf`-rooted identifier with an `acme` derivation is an `acme` candidate — tested, since reading the first segment would open the whole platform namespace to derivations
- [x] ~~`worker.write_lock_timeout` is positive, defaults to 5s and is enforced by the admission worker~~ — **the key is gone.** It bounded the family advisory locks, which T15 retired; the `entity_write_order` claim's wait is the database's own (`lock_timeout`, `innodb_lock_wait_timeout`, `busy_timeout`) and a gear-side budget is not expressible — see SPEC §8.1 and the `TxConfig` ask in §4

**Verification:**
- [x] `cargo test -p cf-gears-types-registry` — 255 lib + 259 integration tests on SQLite; the operator-facing configuration boundary has 21 `config_test.rs` cases
- [x] Table-driven test over the four policy entries in SPEC §10.3 plus the resolution rules, manual `vec![]` + loop (no `rstest`)
- [x] Test: a more specific `allowed_vendors` **replaces** a less-specific set rather than extending it — including the pair of candidates that shows it, one excluded inside the narrow region and the same vendor still admitted outside it
- [x] Test: an entry that omits `allowed_vendors` is skipped, and a less-specific entry supplies it. Both parameters are therefore `Option`: collapsing absent onto `[]` / `false` would let a narrow entry silently close what a broad one opened
- [x] Test: a config carrying `tenant_ownable` starts cleanly and does not admit a tenant-owned candidate
- [x] Test: invalid pattern in `registration_policy` fails startup with the region named
- [x] Added: an unknown key is refused rather than ignored (`deny_unknown_fields` is the other half of a typed configuration — a misspelled limit must fail the boot, not silently never apply); byte sizes accept the documented `256KB` / `1MB` forms and a malformed one fails to parse rather than becoming zero

**Corrected at the Checkpoint 1 review.** "Has no reader" was true and
invisible: five keys read as enforced, and `CLOSURE_BOUND`'s comment claimed to *mirror*
`activation_write_set`, so an operator writing 1024 silently got 512. Three changes, no new
enforcement — the enforcement is scheduled and pulling it forward would be guessing at T14's and
T21's shapes:

- every field now says **enforced** or **accepted, not enforced in P0**, and the latter names the
  task that binds it (T14 for the write set, T15 for `max_revalidation_attempts`, T21 for
  `operation_timeout`, T27 for the page-size pair) — the honest
  shape `tenant_ownable` already had;
- `CLOSURE_BOUND` says it is *its own* bound over what a store build **reads**, not SPEC §8.1
  step 4.6's write set, and its error message no longer borrows the other bound's name;
- `config::inert_limit_keys()` collects the keys a deployment moved off their default that P0 does
  not act on, and `init` names them in one `warn!`. Not a boot failure: a P1-ready configuration
  legitimately carries every one of them. The test over it is exhaustive, so binding a key breaks
  that test — which is the reminder to take it out of the list.

**Resolution budgets:**

- [x] `resolution_closure` bounds each candidate or refreshed schema's distinct authored
  resolution graph, including its own document and the candidate overlay. Converging paths
  count once; `x-gts-ref` and unrelated documents in the shared refresh store do not count
- [x] `resolved_document` bounds the canonical UTF-8 bytes of each effective artifact,
  including the conforming schema used by an Instance. Exactly-at-bound passes; exceeding
  either budget refuses admission and rolls back any revision and dependent refresh
- [x] Both settings reject zero at startup and are removed from `inert_limit_keys()`

**Dependencies:** T2
**Files touched:**
- `TR/src/config.rs` — `allow_compatibility_force`, `Limits`, `PolicyEntry`, `WorkerSettings`, `ByteSize`, `ConfigError`, `validate()`
- `TR/src/domain/policy.rs` — NEW, `RegistrationPolicy` + `PolicyConfigError` + `PolicyRefusal`
- `TR/src/domain/policy_tests.rs` — NEW, 16 in-source tests
- `TR/src/domain/mod.rs` — declare `policy`
- `TR/src/gear.rs` — startup validation
- `TR/tests/config_test.rs` — NEW, 21 tests
**Scope:** M

---

### - [x] T7: Acceptance path and operation records

**Description:** The synchronous half of admission: envelope and batch bounds, canonical
identifier check, registration policy, identifier profile, Draft-07 dialect gate, `force`
gate, request fingerprint, `Idempotency-Key` resolution, then one transaction inserting the
operation, its items and the outbox message. Reads no entity state.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] Checks run in SPEC §8.1 order; policy precedes any existence lookup so a refusal cannot probe the namespace. Kept **structurally**, not by review: `validate` takes the request and the config and has no runner, provider or repository in its signature, so mid-validation existence checking would require changing that signature. Steps 1–6 and 8 are here; **step 7 (the ADR-0015 quarantine) is T18's** — it needs T13's reference extractor — and a `TODO(T18)` marks its position between steps 6 and 8 so it lands as an insertion rather than a reordering. Step 6 fail-closes every otherwise-applicable `force` until T17 can compare and persist its provenance
- [x] Policy gates **every accepted candidate**, because P0 accepts only declared creations: a positive `expected_resource_version` is refused (`AcceptanceError::RevisionNotAccepted`) and deletion is refused with the envelope. SPEC §8.1's revision/deletion bypass is deliberately *not* implemented yet — gating on the caller's declared kind while nothing verified the claim let a request name a version and skip the gate outright (found at the Checkpoint 1 review). The bypass returns at T11, together with the commit-side precondition that makes the claim checkable. A refusal names the region and the parameter
- [x] Fingerprint covers canonical body, operation kind, owner, preconditions and each `force` flag — plus dry-run mode, plane, tenant and principal, as one table-driven test over all nine inputs. Every field is **length-prefixed**, so a digest cannot confuse `("ab", "c")` with `("a", "bc")`, and the digest carries a version tag so a future change to its coverage cannot read as a matching replay
- [x] Replay with a matching fingerprint returns the stored operation (`202` non-terminal, `200` terminal); a different fingerprint under the same key returns `409`. The replay test submits a deliberately **reordered** body, because canonicalization is the thing that makes a replay a replay
- [x] Concurrent acceptance on one key resolves via the unique constraint, loser returns the winner after fingerprint verification. The loser re-reads **outside** the rolled-back transaction: on PostgreSQL a constraint violation poisons the transaction, so a re-read inside it would fail for a second, unrelated reason
- [x] `plane = 1`, `tenant_id = NULL`, `principal_id` the named P0 constant with a `TODO`; ceiling C2 (global idempotency namespace) commented on the constant and at the scope-hash call site
- [x] Ceiling C5 (no operation-retention sweep — terminal operations accumulate) commented on `OperationRepo::insert`, with the §3.2 sweep as its upgrade path
- [x] Literal `expected_resource_version: 0` rejected; absent means must-not-exist

**Verification:**
- [x] Gear tests (see [Commands](#commands)): 223 lib + 182 integration tests, of which 11 are `fingerprint_tests.rs`, 26 `acceptance_tests.rs` and 9 `operation_idempotency_test.rs`
- [x] Tests: replay, fingerprint conflict, concurrent acceptance (8 tasks on a file-backed pool), each refusal reason, `0` rejection — plus a dry run then a commit under one key, which is a conflict rather than a replay of the dry-run result
- [x] Test: acceptance issues no read against `entity` — tested by **removing the ability**, not by inspection: the `entity` table is dropped through a second connection to the same file database, a control probe asserts it really is gone, and acceptance still succeeds
- [x] Added: a dispatch failure rolls the whole acceptance back, and a synchronous refusal writes no operation and dispatches nothing

**Added beyond the criteria:** an over-long `Idempotency-Key` refused before the `varchar(255)`
column sees it; an empty batch refused; a negative precondition refused beside the literal `0`;
and T6's `limits.authored_document` enforced on the canonical bytes, which is what gets stored
and fingerprinted.

**Dependencies:** T4, T6
**Files touched:**
- `TR/src/domain/admission/mod.rs` — NEW, `SubmitRequest` / `Candidate` / `Accepted` / `OperationDispatch`
- `TR/src/domain/admission/acceptance.rs` — NEW, steps 1–6 and 8, the transaction, `AcceptanceError`
- `TR/src/domain/admission/acceptance_tests.rs` — NEW, 26 in-source tests
- `TR/src/domain/admission/fingerprint.rs` — NEW, canonical bytes, fingerprint, scope hash, `P0_PRINCIPAL_ID`
- `TR/src/domain/admission/fingerprint_tests.rs` — NEW, 11 in-source tests
- `TR/src/domain/policy.rs`, `TR/src/domain/policy_tests.rs` — vendor from the last named segment
- `TR/src/domain/mod.rs` — declare `admission`
- `TR/src/infra/storage/repo.rs` — NEW `OperationRepo`, `NewOperation`, `NewOperationItem`
- `TR/Cargo.toml` — `sha2`
- `TR/tests/operation_idempotency_test.rs` — NEW, 9 `SQLite` tests
**Scope:** M

---

### - [x] T8: Admission worker — one dependency-free candidate

**Description:** The worker as a plain function of `(operation_id, runner)`: build the unit's
transient store (T5), evaluate one acyclic, reference-free Type Schema candidate against it,
then commit family, entity, revision, current-state projection with materialized artifacts,
`resource_version` and the item outcome. No dependencies, no compatibility, no batching yet.

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] Entry point is directly callable and returns a result; no `sleep`, timer or polling anywhere in it or its tests. **The error boundary is the retry boundary:** `WorkerError` is infrastructure-only, while a candidate that is simply wrong is an `ItemFailure` recorded on its item — `Err(WorkerError)` versus `Ok(_)` with a failed item. Retrying a final decision would burn the outbox's attempt budget forever, so T21's handler becomes a two-line map
- [x] Evaluation happens outside the transaction; the transaction contains only rechecks and writes. In the type signatures, not by convention: `evaluate` takes a runner and opens nothing, `commit_creation` takes a `&DbTx<'_>`
- [x] `type_schema` row is populated at admission — `resolved_schema`, `effective_traits`, `effective_traits_schema`, `resolution_fingerprint` (D3). All three come from `validate_schema`, which is the **only** public route to them: `effective_traits` is `pub(crate)` in the library and `GtsOps::validate_schema` discards the `ResolvedType` it built
- [x] `resolution_fingerprint` is computed over canonical bytes, independent of map iteration order — the same canonicalization the request fingerprint uses, and asserted against the stored artifacts rather than against a literal
- [x] Creation requires the identifier absent; the outcome records `gts_uuid` and `resource_version`. `gts_uuid` is `GtsId::to_uuid()` — `gts-rust`'s deterministic derivation, never a locally reproduced UUIDv5, which is what T4's *tests* used as a stand-in
- [x] The transient store is built inside the invocation and dropped with it; nothing is retained on the worker, the service or the gear between invocations, and there is no post-commit rebuild step

**Verification:**
- [x] Gear tests (see [Commands](#commands)): 232 lib + 190 integration tests, of which 5 are `family_tests.rs`, 4 `artifacts_tests.rs` and 8 `admission_worker_test.rs`
- [x] Test: registering a schema writes exactly one row in each of the five affected tables — `version_family`, `entity`, `type_schema_revision`, `type_schema` and the `operation_item` outcome, plus the operation's own transition to `completed`. `dependency` is deliberately **not** among them: this candidate references nothing and edge extraction is T13
- [x] Test: `resolution_fingerprint` is stable across two computations of identical artifacts, and each of the three artifacts moves it
- [x] Test: creation against an existing identifier fails **terminally** with no revision written — recorded on the item, not returned as a worker error, and through the recheck *inside* the commit transaction, which is the same path a concurrent creation hits
- [x] Test: a second invocation after a committed revision sees it — **partly, and the test says so.** A derivation that consumes the base through a `$ref` needs the base in its closure, and the edges are T13's; so the derived candidate fails, with the reason asserted, and what is proved is that a fresh invocation reads the committed row and its authored document from the database with no carried-over copy. The store-level form is already covered by T5's `two_sequential_builds_each_observe_the_committed_revision`; the end-to-end form arrives with T13
- [x] Added: an unresolvable `$ref` and a document failing its meta-schema are item failures rather than worker errors; an unknown operation UUID *is* a worker error; a failed evaluation leaves all three affected tables empty while the operation still reaches `completed`; a second pass over a completed operation is a no-op

**Not done, deliberately:** `insert_current` is an insert rather than an upsert — moving an
existing pointer is a revision with its own preconditions (T11), and a silent overwrite would
hide a missing recheck. One item is one unit, in `item_no` order; in-batch references and
topological ordering are T19. `run_operation` takes no config, because nothing in T8 reads a
limit — T14 gives it `&Limits`.

**Duplicate delivery is already a no-op**, ahead of T21 asking for it: a completed operation is
recognized and its stored outcomes returned, and an item already terminal from an earlier pass is
skipped. Four lines, and the property at-least-once delivery makes load-bearing — leaving it to
T21 would have meant writing the worker twice.

**Dependencies:** T5, T7
**Files touched:**
- `TR/src/domain/admission/worker.rs` — NEW, `run_operation`, `WorkerError`, `ItemFailure`, outcomes
- `TR/src/domain/admission/unit.rs` — NEW, `evaluate` (no transaction) and `commit_creation` (transaction only)
- `TR/src/domain/artifacts.rs`, `artifacts_tests.rs` — NEW, D3 materialization + 4 tests
- `TR/src/domain/family.rs`, `family_tests.rs` — NEW, family-key derivation + 5 tests
- `TR/src/domain/mod.rs`, `TR/src/domain/admission/mod.rs` — declarations
- `TR/src/infra/storage/repo.rs` — revision / current-state inserts and the four status writes
- `TR/tests/admission_worker_test.rs` — NEW, 8 `SQLite` tests
**Scope:** M

---

### - [x] T9: REST — `POST /entities`, `GET /operations/{id}`, `GET /entities/{entity_key}`

**Description:** The first complete REST path: submit a registration and get `202` +
operation, poll the operation, read the entity. `POST /entities` breaks its old `200`
shape (D10).

Outcome and evidence: the criteria below. The per-task report was folded into these and deleted.

**Acceptance criteria:**
- [x] `POST /entities` returns `202` with operation `Location` and advisory `Retry-After`; `200` only on terminal replay. The `Location` is built from the path the request **arrived on** and is therefore followable under api-gateway's `prefix_path` — the credstore precedent, with one correction to it: `OriginalUri`, not `Uri`, because `apply_prefix` mounts every gear with `Router::nest`, which rewrites away exactly the prefix that has to survive (found at the Checkpoint 1 review). The receipt reports the operation's **real** status — with inline admission a fresh submission is already `completed`, and telling a caller to poll for something that has happened is worse than useless. It comes from `Accepted.status`, which `submit` already knows, rather than from the read-back this used to do: that read cost a second snapshot transaction over two statements and carried a `"pending"` fallback for an operation that had just been committed and so cannot be absent (found at the Checkpoint 1 review)
- [x] `Idempotency-Key` is required; absence is a synchronous refusal. **A toolkit gap:** `OperationBuilder` exposes `path_param` / `query_param` and no header equivalent, so the requirement cannot be declared as an OpenAPI parameter from this gear. The route description states it *and* states that it is undeclared; fixing the builder is toolkit work outside this gear
- [x] Errors are RFC-9457 problem details via `.standard_errors(openapi)`, never raw status tuples — one match arm per `AcceptanceError` variant, exhaustive, so a new refusal reason cannot reach the wire as an opaque `500` by omission
- [x] Routes are `/types-registry/v1/...` and `.authenticated()`; DTOs live only in `api/rest/dto.rs`
- [x] Added at the review gate: the four wire vocabularies — operation status and kind, item status, entity kind, lifecycle status — are `#[api_dto]` **enums**, not `String` fields whose admissible values lived in a docstring. A `String` publishes an unconstrained `string` in the served `OpenAPI`, so a generated client gets no vocabulary and a value this gear never emits type-checks against it; the five `*_str` helpers are gone with it (found at the Checkpoint 1 review)
- [x] `GET /entities/{entity_key}` accepts a GTS identifier or a `gts_uuid` — classified by `EntityKey::parse` in the **domain**, so a future gRPC adapter with the same field gets it for free
- [x] These routes are the **platform-plane** API for global entities: they keep the authentication they have and no handler assumes a tenant scope. `.anonymous()` is **not** used. Ceiling C8 is commented where the routes are registered. **Corrected after the PR's contract check:** the routes were initially `.exposed()`, but `exposed` gates gateway visibility, not `OpenAPI` inclusion — every registered operation lands in `docs/api/api.json` either way — so it bought a published interim surface with no consumer (the database path has none until T24) and cost a stale `api.json`. The v2 routes are now **internal-only** (no `.exposed()`): documented in the spec and invisible to the gateway. T24a changes their paths but must not expose mutations while C8 remains open
- [x] **Handlers are mapping steps only.** `RegistryService` has three methods taking and returning domain values with no `StatusCode`, `HeaderMap` or `Json`. The identifier-versus-UUID classification stays in the domain. The request's `expected_resource_version` is persisted as the worker precondition (`0` means must-not-exist) but is not echoed by the public operation outcome. The created `revision_no` likewise remains internal admission provenance; only the resulting `resource_version`, which future writes accept as their precondition, crosses that response boundary. `Idempotency-Key` is read from the header and passed as a parameter

**Verification:**
- [x] `cargo test -p cf-gears-types-registry` — `api_rest_test.rs` via `Router::oneshot`, 10 cases, driving the **real** `register_routes` with a stub `OpenApiRegistry` rather than a hand-built router: a bare router would pass while a route was registered at the wrong path or without its auth stage. 226 lib + 195 integration tests pass
- [x] Manual: `curl` the three routes against `make example`; `/cf/docs` renders them — **done after this task, at Checkpoint 1; the blocker recorded here was the invocation and is retracted.** `oagw` (`api_egress`) is a non-optional dependency of `cf-gears-example-server` while `static-tr-plugin` sits behind the `static-tenants` feature, so a bare `cargo run --bin cf-gears-example-server -- --config config/quickstart.yaml run` builds the tenant resolver with **no plugin at all** and `oagw`'s root-tenant lookup is the first thing to notice it. `make example` passes `E2E_ARGS`, which defaults to `config/e2e-features.txt`, and `static-tenants` is in that set: with it, all 25 gears boot, `/cf/health` answers `200`, the three routes behave, and `/cf/docs` renders them. The `config/e2e-local.yaml` half stands as written — that is the old in-memory path T24 deletes. What the live run found: the `202`'s `Location` omitted api-gateway's `/cf` prefix and so could not be followed verbatim — **fixed at the review gate**, and pinned by a test that nests the real router under `/cf` and follows the receipt it hands back

**Superseded by T9a.** This task repointed the two existing v1 routes at the database path; T9a
restores v1 and moves this surface to `/types-registry/v2/`, for the reasons in `plan.md` P12. The
DTOs, handlers and tests below are the ones T9a re-registers under v2 — nothing here is discarded,
only re-addressed.

**Three interim shapes, each marked in code.** Admission runs **inline** until T21 starts the
outbox worker — not a throwaway path, because seeding does exactly this permanently (SPEC §8.1),
and the dispatch call still happens inside the acceptance transaction through `NullDispatch`, so
T21 changes one implementation rather than the transaction's shape. The scope is `allow_all`
(ceiling C6). Ceiling C8 is commented at the routes.

**The database is optional.** `ctx.db()` rather than `db_required()`: `no-db.yaml` and `--mock`
bind none, and failing their boot for a path they do not use would be a regression. Where none is
bound the routes are registered and answer `503` with a problem document naming the cause — a
`404` would suggest the API changed — and `init()` logs the config key to set.
`config/quickstart.yaml` and `config/e2e-local.yaml` now bind one: this is the binding T2
deferred to *"the first slice that reads these tables"*, and before this task the migration ran
in no deployment at all.

**Dependencies:** T8
**Files touched:**
- `TR/src/domain/registry_service.rs` — NEW, `RegistryService`, `EntityKey`, record types, `ServiceError`
- `TR/src/api/rest/routes.rs` — the three routes, ceiling C8 comment
- `TR/src/api/rest/handlers.rs` — three new handlers; the two replaced ones deleted
- `TR/src/api/rest/dto.rs` — the submit-then-poll DTOs and mappings; the old shape's four DTOs deleted
- `TR/src/api/rest/error.rs` — `From<ServiceError>` / `From<WorkerError>` / `From<AcceptanceError>`
- `TR/src/domain/admission/mod.rs` — `NullDispatch`
- `TR/src/infra/storage/repo.rs` — `EntityRepo::find_by_gts_uuid`, `TypeSchemaRepo::find_current`
- `TR/src/gear.rs` — wire the database-backed service when a database is bound
- `TR/Cargo.toml` — `tower` dev-dependency
- `TR/tests/api_rest_test.rs` — NEW, 10 route tests
- `TR/tests/registration_tests.rs`, `TR/tests/query_tests.rs` — tests of the deleted handlers removed
- `config/quickstart.yaml`, `config/e2e-local.yaml` — bind a database to types-registry
**Scope:** M

---

### - [x] T9a: Restore the v1 REST contract; the async surface moves to `/types-registry/v2/`

**Description:** T9 **repointed** the two existing v1 routes instead of adding new ones, which
breaks the invariant this plan set for itself (risk table: *"The DB path has no consumer until
T24; no dual-write"*). Three consequences, all observable today:

1. `POST /v1/entities` changed its **body shape** and gained a required `Idempotency-Key`.
   Every existing caller gets `400`/`422`, so this is not the "expects `201`, gets `202`"
   break T28 was scoped for.
2. **Cross-gear registration over REST is functionally broken.**
   `testing/e2e/gears/oagw/helpers.py:83` and
   `testing/e2e/gears/account_management/conftest.py:182` register over REST and then resolve
   through `TypesRegistryClient` (`account-management/src/infra/types_registry/`,
   `usage-collector/src/domain/service.rs`). Those gears now **write to the database and read
   from process memory**. No e2e change fixes this — only T24 does.
3. One resource, two sources of truth: `GET /v1/entities` (list) answers from memory,
   `GET /v1/entities/{entity_key}` from the database.

Restore both v1 routes from `main` verbatim and register T9's surface under
`/types-registry/v2/`. Everything v1 needs is still in the crate —
`TypesRegistryService::{register_validated, get, is_ready}` (`domain/service.rs:66,108,144`),
`InMemoryGtsRepository`, `GtsEntityDto` — only two handlers and four DTOs were deleted, and they
come back unchanged. v2 is **interim by design**: T24 deletes v1 with the repository it reads,
and T24a promotes v2 onto the v1 paths, so P0 ends on one version.

**Acceptance criteria:**
- [x] `POST /types-registry/v1/entities` is contract-identical to `main`: `operation_id` `types_registry.register`, body `{"entities":[…]}`, `200`, `RegisterEntitiesResponse` with its summary, served from the in-memory service behind `is_ready()`
- [x] `GET /types-registry/v1/entities/{gts_id}` restored: `operation_id` `types_registry.get`, in-memory, unchanged DTO
- [x] The four deleted DTOs — `RegisterEntitiesRequest`, `RegisterEntitiesResponse`, `RegisterSummaryDto`, `RegisterResultDto` — are **restored from `main`**, not re-derived: a re-derived DTO is a second wire change nobody asked for. Lifted out of `git show main:` rather than retyped, so the wire shape cannot drift by a field name
- [x] `GET /types-registry/v1/entities` (list) is untouched; it never moved
- [x] The async surface is `POST /v2/entities`, `GET /v2/operations/{operation_id}`, `GET /v2/entities/{entity_key}`, keeping T9's `operation_id`s — `submit_entities`, `get_operation`, `get_entity`, distinct from v1's `register`, `list`, `get`
- [x] **No route straddles the two stores:** no v1 handler takes `RegistryService`, no v2 handler takes `TypesRegistryService`. Grep-checked — three handlers take `Extension<Arc<TypesRegistryService>>` (`register_entities`, `list_entities`, `get_entity`) and three take `Extension<Option<Arc<RegistryService>>>` (`submit_entities`, `get_operation`, `get_entity_by_key`); no handler takes both
- [x] **No dual-write and no fallback read.** Pinned by `a_v1_registration_is_absent_from_v2_and_the_reverse`: a v1 registration reads `200` on v1 and `404` on v2, a v2 admission `200` on v2 and `404` on v1. The two directions run against the *same* routes, so the `404` cannot be a missing route rather than an honest miss — the test would fail on the companion `200` first
- [x] `RegistryService` stays per-database-optional as T9 left it: with no database bound, v2 answers `503` and v1 works. Both halves are tested — `without_a_database_the_routes_report_service_unavailable` for v2, and `without_a_database_the_v1_routes_still_serve` for v1's register, get and list
- [x] Route paths come from one constant per version, so T24a's promotion is a constant change rather than a sweep. `routes::V1` / `routes::V2` are `pub` and the test imports **the same two constants** the routes are built from — not its own copies, which would drift at exactly the moment the promotion happens
- [x] The tests T9 deleted from `registration_tests.rs` and `query_tests.rs` return with the routes they cover — three and two respectively, and both files now `diff` clean against `main`
- [x] SPEC §10.2 is amended: the v1 break is **withdrawn** for the T9a–T24 window and reinstated at T24a, naming both tasks. The route table there is labelled as the post-T24a surface, so it is not read as describing today

**Verification:**
- [ ] `make e2e-local` green **without editing a single e2e file** — that is the criterion, because the task's whole point is that the suite did not need migrating yet. **Blocked by a pre-existing T1 defect, not by this task** (see the Checkpoint 1 item below): the run never reaches pytest, because types-registry's `post_init` fails first. No e2e file was edited, which is the half of the criterion this task owns and which holds
- [x] Attribution done rather than assumed: `make e2e-local` was run on a **stashed tree at HEAD**, without any T9a change, and failed with `MAKE_EXIT=2` and the byte-identical signature — same two identifiers, same `Ready commit failed with 2 errors`. The suite was already red at Checkpoint 1
- [x] `cargo test -p cf-gears-types-registry` — **443 passed, 0 failed** across 19 suites (426 before this task; +15 in `api_rest_test`, +5 restored, and the arithmetic differs by the two suites whose counts moved with the restores)
- [x] The v1/v2 split is **asserted at router level instead of checked by hand** — `a_v1_registration_is_absent_from_v2_and_the_reverse` drives the real `register_routes`, so it covers the same ground as the manual pass and cannot rot. The manual criterion is satisfied by it plus `make e2e-local`, which boots the real server over the unmodified suite
- [x] **Both versions reach the generated document with six distinct operation ids** — also a test rather than a manual look, because the failure mode is invisible in the router: `OperationBuilder` registers two routes under one `operation_id` without complaint, and the second silently replaces the first in the document a generated client is built from. `TestOpenApi` now records `(method, path, operation_id)` and `both_versions_are_declared_with_distinct_operation_ids` asserts the whole set. **Mutation-checked:** giving v2's read v1's `types_registry.get` fails it with the colliding pair named
- [x] Added, not in the original criteria: a pre-existing `clippy::doc_markdown` error in `domain/admission/fingerprint.rs:73` (unbackticked `UUIDv5`, from the Checkpoint 1 commit) blocked `-D warnings` for the whole gear. Fixed here — one word of backticks — because a slice cannot be verified against a red gate it did not cause

**Dependencies:** T9
**Files likely touched:**
- `TR/src/api/rest/routes.rs` — v1 restored, T9's three routes moved to v2
- `TR/src/api/rest/handlers.rs` — `register_entities` / `get_entity` restored from `main`
- `TR/src/api/rest/dto.rs` — the four DTOs restored
- `TR/tests/api_rest_test.rs`, `TR/tests/registration_tests.rs`, `TR/tests/query_tests.rs`
- `docs/p0/SPEC.md` §10.2
**Scope:** S — a revert plus a prefix move; no domain code changes

---

### - [x] T10: Registered Instances

**Moved into Phase 1** from Phase 2. Two reasons. Instances are what the platform actually
pushes today — P4 counts *"roughly eleven plugin gears"* already registering their well-known
Instances from their own `init()` — so they are on the critical path to T24 and are the longest
pole, not a widening. And T9's surface accepts an Instance and then fails it in the **worker**
(`StoreBuildError::UnsupportedKind` → `WorkerError::StoreBuild` → opaque `500`), which is a
retryable class for a decision that is final; building the feature closes that hole instead of
adding a refusal for it.

**Description:** Extend admission to registered Instances: `instance_revision`, `instance`
current pointer, conformance to the Type Schema identified by the identifier prefix through
the last `~`, and the immutable schema-revision pair.

**Three companions come with the move, and each is load-bearing rather than tidy-up.**

1. **The closure gains an identifier-derived component.** `GtsStore::validate_instance` resolves
   the instance's `type_id` *out of the store*, so the parent Type Schema must be loaded — and
   today `DependencyRepo::closure` walks the `dependency` table only, which nothing writes until
   T13. But a derivation base is not an edge: `GtsId::chain_ids()` and `get_type_id()` are pure
   functions of the identifier, needing no table at all. So the closure seeds its worklist with
   the candidate's chain **and** its edge targets. This is why the move is cheap, and it also
   repairs a Phase 1 defect: `admission_worker_test.rs::a_second_invocation_sees_the_first_ones_committed_revision`
   currently asserts that a derived Type Schema **fails**, and the comment blames T13's missing
   edges — half right, since `validate_schema` walks `chain_ids()` and would have found the base
   with no edge table involved. T13 supplies the `$ref` targets needed by the forward closure and
   the three direct edge kinds needed by T14's reverse walk. `x-gts-ref` is neither resolved nor
   represented by an edge.
2. **`instance` and `instance_revision` acquire their four edits** — entity, repository with its
   mapper, port trait and types, and the forwarding block in `store.rs`. T2's migration already
   created both tables, so no migration changes.
3. **T12's *kind* rule comes with T10; shape and contiguity stay in T12.** A Type Schema
   `…ns.thing.v1~` and an Instance `…ns.thing.v1` derive the **same** `family_key`
   (`family.rs` normalizes the trailing `~` away — see `family_tests.rs`), so the first Instance
   can land in a family whose members are Type Schemas. Unguarded, that produces a family row a
   later task's invariant assumes cannot exist.

**Acceptance criteria:**
- [x] An Instance records the exact Type Schema revision that validated it — `(type_schema_entity_id, type_schema_revision_no)` on `instance_revision`, read in **the same snapshot** as the transient store so the recorded pair is the one that actually validated; a second read could see the schema revised in between
- [x] An Instance whose conforming schema is absent fails retryably, not terminally — `WorkerError::ConformingTypeAbsent`, checked **before** validation so the failure names the real cause rather than reporting a missing schema as a content fault
- [x] A minor or major 0 in the Instance identifier's last segment is refused at acceptance — **already built and tested in T7**; `an_instance_identifier_must_name_a_stable_major_without_a_minor` covers both halves plus the Type Schema contrast, so this task adds nothing
- [x] `instance` carries only the current-revision pointer — no derived artifact. The asymmetry with `type_schema` is documented on the entity so it is not "fixed" later: an Instance has no derived state, its value is authored and its schema revision is immutable and pinned by `ON DELETE RESTRICT`, so there is nothing that could change without a new revision and nothing to fingerprint
- [x] `entity_kind` is derived from the identifier, not passed in. Stronger than removing the literal: `EvaluatedOutcome` is an **enum** whose variant carries the kind-specific payload (artifacts for a schema, the revision pair for an Instance), and `entity_kind()` reads it off the variant. Two `Option` fields beside a `kind` discriminant would have made a mismatch representable; here it is a compile error
- [x] The transient store carries both kinds: both `UnsupportedKind` refusals are deleted and the dialect gate is keyed on the identifier's `~`. `type_id` is passed to `GtsEntity::new` **explicitly** from `GtsId::get_type_id()` with a `None` config — letting content name the conforming type would allow a value to claim conformance to a type it was not registered under, which is a validation bypass, not a convenience
- [x] The closure reaches a candidate's derivation chain with **no `dependency` row present** — `closure` seeds its worklist with `GtsId::chain_ids()`, and `missing_roots` is still computed over the original roots so a chain member the seed added is not reported as one. The Phase 1 test is inverted, and its old comment was half wrong: it blamed T13's missing edges, but a derivation base is a pure function of the identifier
- [x] A family holds one kind — `family_kind_conflict`, checked under the family lock and only for a family that already existed. Distinct from `already_exists` because the identifier *is* free; saying otherwise sends a caller looking for a conflicting entity that does not exist. Both orders are tested
- [x] No `$ref` target is expected to resolve in T10; document-reference support belongs to T13 and is verified there by `a_ref_outside_the_chain_is_admitted`. An `x-gts-ref` target is never resolved or seeded because validating the keyword reads no target document.
- [x] **An admitted Instance reads back with its value.** *Added after the task was marked done — see the correction below.* `GET /v2/entities/{entity_key}` returns either kind's authored document under the single public field `content`. The immutable `revision_no` remains an internal current-pointer and provenance detail; the public entity exposes `resource_version`, which is the token future writes accept. The three `effective_*` artifacts stay absent for an Instance, and that is the contract rather than the same omission: an Instance has no derived state, so there is nothing to materialize

**The read path was missed, and the task's criteria are why.** Every criterion above is about
admission — the write path, the transient store and the family rule — and none names the public
read. `RegistryService::entity()` was built at T9, when only Type Schemas existed, and kept asking
`TypeSchemaStore::{find_current_schema, current_documents}` for **both** kinds. An Instance has no
row in either table, so an admitted Instance answered `200` with `content: null` while its
operation reported `succeeded` — the value was durable, correct, and unreachable through the
API. `InstanceStore::current_values` already existed and was already
correct; it had one caller, `gts_store.rs`, which is the **transient validation** store and not a
read path. `api_rest_test.rs` contained no Instance case at all, which is why 454 green tests said
nothing about it.

The read now branches on the row's kind into a `CurrentState` enum — the same shape
`EvaluatedOutcome` has on the write side, so "an Instance carrying a resolved schema" is not a
representable value rather than a mismatch a later edit could introduce. One statement, not two:
`current_values` returns the pointer's revision number with the value it points at; the service
uses that pairing internally and does not publish the revision number.

**Consequence for later tasks, since it would have surfaced there as a regression.** T23's
`get_instance` and T27's `batchGet` both sit on this method; the SDK helpers that hydrate a
content-free page through `batchGet` would have returned pages of null values.

**Verification:**
- [x] Gear tests, all three backends — 454 on `SQLite`, and `make test-types-registry-db` green: 4 container tests, migration and repository primitives on both `PostgreSQL` and `MySQL`
- [x] Test: Instance value violating its schema is refused — `invalid_value`, distinct from `invalid_schema`: the schema is fine, the value is not
- [x] Test: `fk_tr_instance_revision_schema` prevents dangling schema-revision references — `instance_revision_cannot_reference_a_missing_schema_revision`. Foreign keys are enabled explicitly on that connection, because `SQLite` does not enforce them by default and the test would otherwise pass while proving nothing
- [x] Test: an Instance admits against a Type Schema committed by an **earlier** operation, with the `dependency` table asserted empty
- [x] Test: a derived Type Schema admits against a committed base, with the `dependency` table empty
- [x] Test: Type Schema then Instance under one `family_key` — second is refused; and the reverse order
- [x] Test: an admitted Instance reads back over the **real routes** with its authored value under `content`, without exposing internal `revision_no`, and with the three `effective_*` artifacts asserted absent — `api_rest_test.rs::an_instance_reads_back_with_its_authored_value`, which polls the operation first so a refused candidate cannot be mistaken for a value the read failed to reach. Plus the same Instance by its Registry Reference, pinning that the kind branch is chosen by the row and not by how it was found. Both are genuine RED→GREEN: they failed on `content: null` before the fix. Re-run on all three backends — 457 on `SQLite`, `make test-types-registry-db` green

**Interim, and stated rather than discovered later:** *"fails retryably"* is exercisable only as a
unit call on `run_operation`, because there is no outbox to redeliver until T21 and inline
admission surfaces a `WorkerError` to the caller as a `500`. That is the existing behaviour of
every `WorkerError` today, not something this task introduces; T21 makes it a redelivery.

**Dependencies:** T8 (commit path), T9a (so the surface it lands on is v2)
**Files likely touched:** `TR/src/infra/storage/entity/{instance,instance_revision}.rs`, `TR/src/infra/storage/repo/{instance_repo,dependency_repo}.rs`, `TR/src/domain/admission/unit.rs`, `TR/src/domain/gts_store.rs`, `TR/src/domain/family.rs`, `TR/src/domain/ports.rs`, `TR/src/infra/storage/store.rs`, `TR/tests/instance_test.rs`
**Scope:** M — at the top of M with the three companions; if the closure change grows past the chain seed, split it out rather than letting this become an L

---

### Checkpoint 1 — proves the architecture

Outcome and evidence: the criteria below.

- [x] A fixture Type Schema registers over REST, the operation reaches `completed`, the entity and its resolved artifacts are readable — as a test (`api_rest_test.rs::a_registration_is_accepted_polled_and_read_back`, driving the real `register_routes`) **and now at runtime**: `POST /cf/types-registry/v1/entities` `202` → operation `completed` with one `succeeded` item → entity `active`, `rv=1`, all four artifacts materialized, readable by `gts_id` and by `gts_uuid`. **T9's boot blocker was the invocation, not the code, and is retracted:** `oagw` is a non-optional dependency of the example server while every tenant-resolver plugin sits behind a cargo feature, so a bare `cargo run` compiles the resolver with no plugin and `oagw` is the first to notice. With `--features "$(cat config/e2e-features.txt)"` — which is what `make example` passes — all 25 gears boot and `/cf/docs` renders the four routes
- [x] Durable registration state survives closing and reopening the database pool byte-identically — `TR/tests/restart_persistence_test.rs`, two tests. One admits both a Type Schema and an Instance, drops the service/provider/pool, reopens the SQLite file, re-runs test migrations, and compares whole `Model` values in stable primary-key order across all eight affected tables; it then proves both entities are readable through a fresh service and that the persisted idempotency record replays without a write. The other pins the pre-T21 crash-window recovery of a committed non-terminal operation. This is deliberately not called a process-restart test: real `TypesRegistryGear::init`, startup seeding and a new process remain T30's e2e/manual obligation
- [x] Consumers untouched: the old `TypesRegistryClient` is still served from its existing in-memory repository; full workspace tests pass — **10593 passed, 368 skipped, 0 failures** (`cargo nextest run --workspace` minus the two macro crates, as `make test-no-macros` does). Structurally, the branch touches four files outside the gear — `Cargo.lock`, `Cargo.toml` (gts 0.11.0 → 0.12.0) and the two configs — and **not one file in `types-registry-sdk`**; the gear still holds `service` and `local_client` beside the new `registry`
- [x] The new path holds no entity state between admissions: the store is built per unit and dropped, and the entity read in the first item above comes from the database — `RegistryService` has no store field and `grep ArcSwap src/gear.rs` finds nothing; `build_store` / `load_unit_store` are free functions returning an owned `UnitStore`, so there is no `self` to retain it in. T5's `two_sequential_builds_each_observe_the_committed_revision` proves the consequence, and the `503`-without-a-database case shows the read really is a database read
- [x] Gear tests green on SQLite, PostgreSQL and MySQL (see [Commands](#commands)) — 423 tests on SQLite, and the **first ever** container run of the two backend suites, Docker having been down for T1–T9. It found a real defect: `sqlx` binds `Uuid` as 16 raw bytes on both non-native backends, so every uuid write failed on MySQL's `CHAR(36)` and was silently stored as a blob in `SQLite`'s `TEXT`. Fixed to `BINARY(16)` / `BLOB` + `ck_tr_*_uuid_len`
- [x] `make dylint` clean — green for this gear **and for the whole workspace**. This is the phase-end run for Phase 0 and Phase 1 (P13), and the first one: 26 violations were standing across T1–T9, and because `file-storage`, `mini-chat`, `chat-engine` and `oagw` reach types-registry through `authz-resolver`, they could not be linted past it either. DE0708 (2) — `sha2` replaced by inline FNV-1a where the input is server-side and by `aws-lc-rs` SHA-256 where it is client-controlled; no allow-list entry spent. DE1302 (10) — wire details composed from variant fields, infrastructure causes logged and answered opaquely. DE0301 (14) — the domain now names `domain::ports` and `domain::enums`, never `crate::infra`
- [x] **The T1 defect that made `make e2e-local` red is fixed.** types-registry's `post_init` used to fail before pytest started:

  ```
  GTS validation error gts_id=gts.cf.core.am.tenant_type.v1~cf.core.am.customer.v1~
    ... is not compatible with base 'gts.cf.core.am.tenant_type.v1~':
    Schema at '$' adds required properties: ["id"]
  ```

  Reproduced identically on a stashed tree at HEAD, so it predated T9a. **T1's failure mode arriving late** — SPEC §7 named the class, and T1's three measured populations (118 runtime entities, 797 doc/JSON files, macro literals) cover no schema embedded in a YAML config or posted by an e2e fixture.

  **Root cause, and it was not what the error suggested.** AM's abstract envelopes `tenant_type.v1~` and `tenant_metadata.v1~` declared a closed root with **no extension point at all** — only an inert `id` field present to satisfy the `gts-macros` base-struct contract, which being non-`Option` landed in `required`. Their derived types are authored as runtime JSON, so they never composed the base either. Under gts 0.12.0's corrected directional check a derived type must be *included in* its base, and a derived that omits a required property accepts documents the base rejects.

  **Two config-level fixes were tried and both are wrong**, which is worth recording because each looks right:
  - `required: ["id"]` on the derived passes the derivation check and **silently weakens validation** — with no `properties.id` in scope, `{"id": 123}` and `{"id": "not a gts id"}` both validate. Measured, not argued.
  - `allOf: [{$ref: base}]` composes correctly and then **breaks real data**, because the base's `required: ["id"]` and `additionalProperties: false` reach the payload: a metadata value `{"environment": …, "owner": …}` is refused.

  **Fixed at the root, per GTS spec §4.4.1** *"closed envelope with designated open containers"*. Both envelopes now close their root and declare an open `payload` container, and the inert `id` is no longer `required` — no instance can supply a meaningful value for a field that exists to satisfy a macro. The canonical shape is `gts-macros-cli`'s `BaseEventV1<P>`.

  **The wire is unchanged.** AM stores and exposes the payload content, so `MetadataSchemaRegistry::validate_value` composes the envelope (`{"payload": value}`) at the one seam that validates a whole metadata document. `gts_validation::validate_property_value` checks individual properties and is unaffected. AM's REST contract, its domain models and the e2e suite keep their shapes
- [x] `make dylint` **re-run** after T9a and T10 — workspace-wide, exit 0, zero findings. Unlike the first run at Checkpoint 1, nothing had accumulated: a phase is a short enough window that the two tasks since carried no layering debt (P13)
- [x] **v1 is intact and the new surface is additive (T9a).** Both v1 routes are restored verbatim from `main` and the async surface sits under `/v2/`; three handlers take `TypesRegistryService`, three take `Option<Arc<RegistryService>>`, and neither falls back to the other. `make e2e-local` is green with **no e2e file edited for T9a** — the one e2e file this branch touches, `account_management/conftest.py`, belongs to the envelope fix above and would have been needed with or without T9a
- [x] **Review follow-up: unsupported dry-run fails synchronously, and replays are explicit.** Until T20 supplies a rollback-only evaluation transaction, `dry_run: true` is rejected during admission with a canonical `400` field violation, before an operation can be created or stranded in `running`. Successful idempotency replays now include `Idempotency-Replayed: true`; first submissions omit it. Domain and REST regression tests pin both contracts
- [ ] **Human review — everything after this widens the path rather than reshaping it.** Five open items, none of them a failing check:
  - **Another gear owns part of `/types-registry/v1/*`.** `resource-group` registers five routes — `POST|GET /types`, `GET|PUT|DELETE /types/{code}` — inside this gear's service namespace, from `gears/system/resource-group/.../api/rest/routes/types.rs`. T27 and T28 widen that namespace, so a collision waits for whichever gear registers a conflicting path first. Decide: report to the resource-group owners now, or carry it as a known hazard into T27/T28
  - ~~**`Idempotency-Key` cannot be declared in OpenAPI.**~~ **Retracted — the claim was false and is now fixed.** `ParamLocation::Header` exists and `openapi_registry.rs:200` already maps it onto utoipa's `ParameterIn::Header`; the generic `OperationBuilder::param(ParamSpec)` declares it. What misled us is that there is no `header_param` convenience beside `path_param` / `query_param`, so the capability is discoverable only by reading the enum. `POST /v2/entities` now declares the header as a required parameter, pinned by `the_idempotency_key_header_is_declared_as_a_required_parameter` and mutation-checked. The remaining toolkit gap is the missing convenience method — filed upstream as constructorfabric/gears-rust#4614
  - **T2's three lowering decisions were flagged *worth review* and never signed off:** MySQL `DATETIME(6)` rather than `TIMESTAMP(6)`; three extra boolean-domain CHECKs on SQLite and MySQL; MySQL's four indexes declared inline as `KEY`
  - **The migration changed after T2 was marked done** (the uuid binding). Nothing to migrate forward — no deployment had run it — but the "done" marker moved
  - **T10's marker moved too, and the reason generalizes.** The public read returned `content: null` for an admitted Instance; fixed, with the correction written into T10. What is worth deciding rather than just noting: T10's criteria covered admission exhaustively and never named the read, and the same asymmetry stands wherever a task extends the write path — T11, T13 and T20 each add state that `RegistryService::entity()` must then be able to return. Consider a standing criterion for those three: *whatever this task makes storable is readable through the public surface, with a test on the route*. **T11 adopted it** — the criterion is written into T11's verification and discharged by `a_revision_is_readable_through_the_entity_route`; T13 and T20 still need the same, and the decision to make it standing rather than per-task is still yours
  - **Five gear groups have their GTS declarations proven at compile time only**, never admitted into a live registry: `bss-rate-provider` and its plugins, `usage-collector` and its TimescaleDB plugin, `tr-authz`, the OoP calculator example, and `chat-engine` (which `make gts-docs` excludes by policy). They are outside `config/e2e-features.txt`, so the runtime dump covered 34 of the repository's ~40 Type Schema declaration sites. Cheap follow-up once the configs they need exist: repeat the dump with those features enabled

---

## Phase 2 — Revisions and concurrency

**What a new table costs, from here on.** Refactored after Checkpoint 1, so it is not what
T2–T9 did: repositories in `infra/storage/repo/` take and return the row and input types in
`domain/ports.rs` and map their own `SeaORM` models at the edge — one file per repository. A
new table is therefore four edits, not two: the entity, the repository (with its mapper), the
port trait and its types, and a forwarding block in `infra/storage/store.rs`, which holds no
mapping and exists only because the domain wants one `Arc<dyn Stores>`. `entity::Model` must
not leave `infra/storage/repo/`; the pairs of identical `repo::New*` / `ports::New*` structs
that shape removed are the failure mode to avoid re-creating.

**Every port method takes `&DbTx<'_>`.** Not a runner, and not a `&dyn DBRunner` — a dyn
runner can be *coerced* to (sealing prevents implementing the trait, not coercing to it,
which is why mini-chat's `OutboxEnqueuer` takes one) but cannot be *queried* with:
`SecureSelect::one` carries an implicit `Sized` bound that `toolkit_db::outbox` opted out of
(`&(impl DBRunner + Sync + ?Sized)`) and the secure query API did not. So the domain opens the
transaction and hands it down. A new port method that wants a pooled connection is a sign the
call belongs outside the ports.

**A multi-statement read opens `ports::snapshot_read()`; a single-statement one opens a plain
`transaction()`.** The isolation level is the substance, not the transaction: PostgreSQL
defaults to `READ COMMITTED`, where every statement takes a fresh snapshot, so wrapping two
reads in a bare `transaction()` leaves the tear exactly where it was. `snapshot_read` is keyed
on `Db::db_engine()` and returns `TxConfig::default()` for SQLite — the toolkit's doc says
SQLite *"maps other levels to `Serializable`"*, but `SeaORM` does not map them, it logs `WARN`
and ignores them, so an unconditional request put two `WARN` lines in the log per read. SQLite
loses nothing: its reader holds a WAL snapshot or a shared lock for the transaction's duration.
`commit_write()` is the mirror image — `READ COMMITTED`, so a recheck sees what another
admission just committed.

### When a domain concept becomes a directory

`domain/` is one file per concept. `admission/` is a directory because the operation pipeline
has six modules; nothing else has earned one yet, and a directory holding one file is the
premature half of this decision rather than the tidy half.

The grouping axis is **the concept** — which is also its table in `database.sql` — and the
tasks below are where three of them acquire a second file. Take the directory then, not before
and not later; moving three files at T12 is cheaper than moving seven at T29, and leaving them
flat is how the root reaches 25 files.

| Concept | Directory at | Contents then |
|---|---|---|
| `family/` | **T10 + T12** | `key.rs` (today's `family_key`), `rules.rs` (kind at T10, shape and contiguity at T12), tests |
| `dependency/` | ~~T13 + T14~~ | **Not taken.** The traversal turned out to be SQL, so there was no second file to put beside `extraction.rs`: `domain/dependency.rs` stays one pure file and T14's admission step went to `domain/admission/refresh.rs`. See T14's *Not `TR/src/domain/dependency/`* |
| `compat/` | **T17 + T18** | `baseline.rs` (selection + resolved comparison), `derivation.rs` (chain + major-0 quarantine), tests |

Staying flat: `policy.rs`, `artifacts.rs`, `validator.rs`, `gts_store.rs`, `enums.rs`,
`error.rs`, `ports.rs`, `registry_service.rs`, `seeding.rs` — one concept each, the way
`credstore` and `account-management` keep `authz.rs` flat beside their aggregate directories.

**A `rules/` bucket was considered and rejected.** The candidates were `policy`, `family`,
`artifacts`, `compat`, `dependency`, `derivation`, `validator` — but only three of those are
rules (`policy`, `compat`, `derivation`); the rest are derivations: `family_key` is identifier
arithmetic, `artifacts` materializes and digests, `dependency` walks a graph, `validator`
builds a freshness token. The only thing all seven share is having no database and no state,
which is a technical property, not a concept. A directory whose name promises a domain concept
it does not have is worse than a flat list, because the next module that is merely *pure* gets
filed there too.

### - [x] T11: Content revisions and compare-and-swap

**Description:** Second and later revisions of a logical entity: `expected_resource_version`
preconditions, immutable revision insert, current-state pointer move, and the `unchanged`
outcome for authored content equal to current.

**No migration.** T2's `ck_tr_operation_item_state` already carries the `unchanged` arm —
`status = 4` requires `result_revision_no IS NULL` beside a non-null `result_resource_version`
and `expected_resource_version >= 1` — and `OperationItemStatus::Unchanged` already existed in
all three vocabularies (domain, storage, DTO). So the CHECK that makes `unchanged` impossible
for a creation was written before there was any code that could reach it; this task is where
the first row lands in that state.

**Acceptance criteria:**
- [x] Acceptance stops refusing a positive `expected_resource_version` — `AcceptanceError::RevisionNotAccepted` is deleted, along with its REST mapping — **and** the SPEC §8.1 step 3 bypass is restored in the same change: the gate is now `if expected == Precondition::MustNotExist`. What makes that safe is the other half of the same commit: `worker::process_item` dispatches on the item's **stored** precondition, and `commit_revision` refuses an identifier the registry does not hold. The caller's declared kind is therefore *enforced*, not trusted — which is exactly what Checkpoint 1's refusal stood in for
- [x] Update requires `entity.resource_version == expected_resource_version`; mismatch is terminal `precondition_failed` with no silent rebase. It goes through `EntityRepo::compare_and_swap_version` — written and unit-tested at T4 with no caller until now — which became a port method here (its first domain caller, per `store.rs`'s own rule)
- [x] The compare-and-swap returns `Some(next_resource_version)` from the repository and `None` on a lost race. The repository computes `next` with `checked_add` and writes that exact value; the domain never reconstructs the database result or saturates at the numeric ceiling
- [x] A positive precondition on a minor-bearing Type Schema is refused during acceptance: ADR-0004 makes that published contract content-immutable, so a change is registered as the next minor rather than appended as a revision
- [x] Equal authored content yields `unchanged`, creating no revision and not advancing `resource_version`. Both kinds: the rule is shared, the tables are not
- [x] `unchanged` is impossible for a create or a delete, enforced in code as well as by the CHECK. In code it is **structural**: `commit_creation` returns `CommittedUnit` and both revision commit paths return `RevisionCommit`; the early `unchanged` proof is constructed only for `Precondition::Version`. A creation of existing content is `already_exists`, whatever the content
- [x] Content hash is a prefilter only; effective artifacts are excluded from equality. `CurrentDocument` and `CurrentInstanceValue` gained `content_hash` so the digest and the bytes travel together, and the decision is `hash == hash && bytes == bytes` — the digest alone would let a collision swallow a real edit

**The concurrency shape, because it is not the obvious one.** The commit transaction runs at
`READ COMMITTED` (`ports::commit_write`), so a concurrent admission can commit between reading
the entity and reading the current document. For a real revision the compare-and-swap closes
that by construction — the precondition is in the `WHERE`. For `unchanged` there is no write to
put a `WHERE` on, so the precondition is **re-asked** immediately before the item write. Without
it, a pass that read revision N's content while another admission committed N+1 would answer
`unchanged` against content that is no longer current, and never notice: it runs no CAS. With
it, both interleavings are serializable — a re-read that still sees `expected` means the other
admission had not committed yet.

**Verification:**
- [x] Gear tests, all three backends (see [Commands](#commands)) — 514 on `SQLite`, and `make test-types-registry-db` green on `PostgreSQL` and `MySQL`
- [x] Tests: stale version, equal content, content equal to an *older* non-current revision (must create a new revision, ADR-0005) — `TR/tests/revision_test.rs`, eleven tests
- [x] Test: revision numbers are contiguous per entity — three admissions yield `1, 2, 3` with `resource_version` at 3
- [x] Test: a revision in a region the policy has since **closed** is admitted (the restored bypass), while a creation there is still refused — `a_revision_survives_a_region_the_policy_has_since_closed` drives both halves against two compiled policies, and the acceptance unit tests pin the pure half
- [x] **The standing read criterion, applied.** Checkpoint 1's open item asked that whatever a write-path task makes storable be readable through the public route. `api_rest_test.rs::a_revision_is_readable_through_the_entity_route` reads `resource_version = 2`, the new `content` **and** the re-materialized `resolved_schema` over the real router; `unchanged_content_reports_unchanged_on_the_operation` pins the other outcome on the wire
- [x] **New repository primitives covered on the container backends**, not only on `SQLite`. `TypeSchemaRepo::update_current` rebinds `resolution_fingerprint` — a binary column, the exact class of defect the uuid binding was at Checkpoint 1 — in an `UPDATE` rather than the `INSERT` T3 covered; and `mark_item_unchanged` writes the one `ck_tr_operation_item_state` arm no other test reaches. Both added to `repo_backends_test.rs`

**Two design choices worth naming.**

- **`update_current` is separate from `insert_current`, not one upsert.** An insert that finds a
  row means a missing existence recheck; an update that finds none means a missing first
  admission. One upsert makes both bugs silent, so each returns its own miss and the revision
  commit turns an unexpected one into `WorkerError::CurrentStateMissing` — infrastructure, not a
  statement about the candidate.
- **The Type Schema pointer and its artifacts move in one statement.** D3's artifacts are outputs
  of resolving *that* revision, so a row carrying revision N+1 beside revision N's
  `resolved_schema` is a state no reader should see — and two statements would create it.

**Interim implementation window (SPEC C9).** T11 makes database-backed revisions executable before T14's
reverse-impact refresh and T17's compatibility comparison exist. No consumer or v1 cutover may
use this path before those checkpoints (the DB path remains internal until T24). During the
window, minor-bearing Type Schema revisions are rejected structurally, and `force` is rejected
whenever it would have a real check to waive; T17 removes that temporary refusal only when it can
record `compat_forced` truthfully. Major-only Type Schema revisions remain staging-only until T14
and T17 close the dependent-refresh and compatibility gaps.

**Dependencies:** Checkpoint 1
**Files touched:** `TR/src/domain/admission/{acceptance,acceptance_tests,errors,unit,worker}.rs`, `TR/src/domain/ports.rs`, `TR/src/infra/storage/repo/{entity_repo,type_schema_repo,instance_repo,operation_repo}.rs`, `TR/src/infra/storage/store.rs`, `TR/src/api/rest/{error,dto}.rs`, `TR/tests/{revision_test,api_rest_test,repo_backends_test,repo_test,common/mod}.rs`
**Scope:** M

---

### - [x] T12: Version-family shape and contiguity rules

**Description:** The remaining non-stored rules enforced under the family lock: minor shape must
be uniform within a major, and minors must be contiguous from `M.0`. Both are keyed lookups, not
scans.

**The kind rule landed with T10** (P13) — but *inline in the commit path*, not in a `rules.rs`.
This task's plan said it would "take over the file it opened"; there was no such file, so T12
created `family/rules.rs` and **moved** the kind rule into it. Worth recording because the
correction is the point: all three rules now read as one list, rather than one rule buried in
`commit_creation` and two beside it.

**No migration, and no new port.** Both rules are exact lookups through
`uq_tr_entity_gts_id` on an identifier the key module derives, so `EntityStore::find_by_gts_id`
— which already returns tombstones — is the only primitive either needs.

**Acceptance criteria:**
- [x] `vM.n~` refused while `vM~` exists; `vM~` refused while `vM.0~` exists — `family_shape_conflict`
- [x] `vM.n~` with `n > 0` refused unless `vM.(n-1)~` exists — `missing_predecessor`
- [x] A `DELETED` predecessor still counts; the predecessor test is re-asked inside the commit transaction. Both fall out of one primitive: `find_by_gts_id` returns tombstones and every rule runs inside `commit_creation`'s transaction, so there is no way for the two rules to disagree about what a tombstone means
- [x] Family ownership is write-once; the entity's owner columns are a projection maintained under the lock. `NewEntity` now takes `family.ownership_scope` / `family.owner_tenant_id` instead of re-reading the request — a copy taken from the row it is verified against **cannot** disagree with it, which is stronger than verifying two independent readings. Write-once is structural: `create_or_get` has no update path, so this is the only writer of either column
- [x] The predecessor is excluded from `dependency` and from the revision vector — a **negative** criterion, and the only discharge available now: T13 does not write edges yet and T15's vector does not exist. `a_predecessor_is_not_a_dependency_edge` asserts the table stays empty after `v1.0~` then `v1.1~`, so T13 inherits a failing test if it adds the edge
- [x] ~~Family locks use the configured 5s retry budget; timeout is a retryable `503` with `Retry-After`~~ — **retired at T15 with the family locks.** `worker.write_lock_timeout` is gone, and so is the `503`: the write path serializes on the `entity_write_order` row, whose wait only the backend bounds (SPEC §8.1, and the `TxConfig` ask in §4)

**Both rules are scoped to one MAJOR**, as the compatibility chain is, so a family may hold a
major-only `v1~` beside a minor-bearing `v2.0~` (`database.sql`). Three of the twelve table rows
exist only to pin that, because "uniform within a family" is the plausible misreading.

**The pure/impure split is where the risk is.** `version_probe` returns *which identifiers decide*
this candidate — three variants for the three shapes a last segment can have, so "a first minor
with a predecessor" is not representable. `sibling_id` lives beside `family_key` because it is that
function run backwards, and a rule that spells a sibling differently from the way the registry
stores it is a rule that **silently never fires** rather than one that fails loudly. That is what
`rules_tests.rs` is for: six pure tests, including that every probe stays inside the candidate's
own family and that an Instance probes Instance spellings.

**Verification:**
- [x] Gear tests, all three backends (see [Commands](#commands)) — 514 on `SQLite`; `make test-types-registry-db` green on `PostgreSQL` and `MySQL`. No new backend cases: the rules add no new SQL shape, only more `find_by_gts_id` calls, which the suite already covers
- [x] Table-driven test over shape and contiguity combinations — twelve rows, each its own database, `shape_and_contiguity_over_the_combinations`. Genuine RED→GREEN
- [x] Test: family key derivation maps `v1~`, `v1.4~`, `v2~` to one row, and a preceding-segment minor survives verbatim — the pure half in `key_tests.rs` (unchanged from T8), the *one row* half in `one_family_row_holds_every_version_and_owns_its_members`, which also pins the owner projection
- [x] ~~Test: concurrent first registration under two owners yields one winner~~ — **already covered, and deliberately not duplicated on `SQLite`.** `repo_backends_test.rs::family_race_yields_one_row` (eight concurrent callers, exactly one `created`) and `family_race_inside_a_transaction_yields_one_row` (the loser keeps reading in the same transaction) are T4's, on both container backends. A `SQLite` version cannot exist: a second concurrent writer fails the whole transaction with `database is locked` rather than losing a unique-key race — measured, not assumed. `family_test.rs` carries a named placeholder test pointing at the two that do cover it. *"Two owners"* is P1 language; P0 fixes every row to `ownership_scope = 1` (ceiling C6)
- [x] ~~Tests: a creation holds the family lock across its transaction; configured contention returns a refusal~~ — **retired at T15 with the family locks themselves.** The `entity_write_order` claim makes the whole commit transaction exclusive, so the family rules those locks serialized are already serialized; keeping them also inverted the lock order against ADR-0013's purge protocol

**Family-lock gap closed, and later retired.** T12's `worker::lock_families` took toolkit advisory locks in canonical
`family::lock_order` before opening the commit transaction and holds them through every family
rule and entity insert. Acquisition used the then-enforced `worker.write_lock_timeout`; contention
returns retryable `503 Service Unavailable` with `Retry-After`, while any guards accumulated
before either timeout or a fatal acquisition error are explicitly released through the same
helper as the successful commit path. `SQLite` keeps its toolkit-defined DSN-keyed lock scope, so
tests use unique database files where contention matters rather than assuming row-lock semantics
that SQLite does not provide. A timeout leaves the operation non-terminal; recovery is a replay
under the same `Idempotency-Key`, which `submit`'s `!terminal` branch re-drives.

**Dependencies:** T10, T11
**Files touched:** `TR/src/domain/family.rs` → `TR/src/domain/family/{mod,key,key_tests,rules,rules_tests}.rs`, `TR/src/domain/{mod,enums}.rs`, `TR/src/domain/admission/{unit,worker}.rs`, `TR/src/config.rs`, `TR/src/api/rest/{error,routes}.rs`, `TR/tests/{family_test,config_test,common/mod}.rs`
**Scope:** M

---

### Checkpoint 2
- [x] Revisions and CAS behave; family shape and contiguity hold under concurrency (the kind rule landed with T10)
- [x] Gear tests on three backends (see [Commands](#commands))
- [x] `make dylint` — full workspace, once for the phase (P13)
- [x] Human review

**The one decision this checkpoint owed: the family lock is wired, not deferred.** T12 first
raised it as an open gap and left the choice between T15 and a task of its own; the answer was
neither — it landed straight away, in `5240378e4` and `6b836e247`, which is why T12's note above
reads as closed. Family validation and creation are serialized under
`Db::try_lock` with a retry on transient contention, so the two-different-new-members race T12
described is closed for T10's kind rule at the same time. The same work stops a revision
resurrecting a tombstoned entity and separates corruption from contention in the worker's error
vocabulary. T15 therefore inherits bounded retry over the revision vector only — the lock and
its ordering are already there.

---

## Phase 3 — Dependencies and materialization

### - [x] T13: Dependency edge extraction and writes

**Description:** Extract the edge kinds from authored content — `$ref`, immediate derivation
base, Instance conformance — and replace the admitted entity's outgoing rows on each admission.

`x-gts-ref` is not a dependency edge. Validation matches the value string against the pattern
without consulting the registry, the target is not inlined into an effective artifact, and the
keyword gives no deletion-safety guarantee. The managed–external boundary will classify candidate
content when federation is introduced. The three stored edge kinds are `$ref`, immediate
derivation base, and Instance conformance.

**Unchanged by T10's move** (P13). T10 made the *forward* closure reach a candidate's
derivation chain from the identifier, so derived schemas and Instances no longer wait on this
task. What still needs rows here: `$ref` targets, which are not derivable from any identifier,
and derivation/conformance, which stay materialized because T14's **reverse**
walk has no identifier to walk backwards from — the criterion below already says so. Both endpoints are always managed entities. This is also
the same extractor the worker uses for in-batch ordering.

**Writing the rows is not enough — the extractor also runs on the read side.** Rows are
replaced at commit, so they do not exist while the candidate that authored them is being
validated: on a first admission `load_unit_store` walks an edge set that says nothing about
this candidate, and a `$ref` to an entity outside the candidate's identifier chain never
reaches the store. `validate_schema` then refuses a target the registry holds. A revision has
the mirror problem — the stored edges are the *previous* revision's, so a reference the
candidate added is not followed and one it dropped still is. `load_unit_store`'s
"the candidate overlay wins" today covers documents but not edges. The fix is the same pure
extractor, called over the candidate document *before* the closure read, its targets seeded as
closure roots beside the candidate's `chain_ids()`. Found at the PR #4641 review.

**Acceptance criteria:**
- [x] `x-gts-ref` creates no edge and has no reader in the P0 dependency implementation
- [x] Admission replaces only the admitted entity's outgoing rows, through the existing `DependencyRepo::replace_outgoing` — written and unit-tested at T4, with no caller until this task
- [x] Derivation and conformance are materialized even though derivable from the identifier
- [x] `$ref` extraction uses `gts-rust`'s extractor, never a local scan
- [x] Extraction is exposed as a pure function over authored content, callable without a database — `domain::dependency::extract_edges(&GtsId, &Value)`, no clock and no ports in its signature, which is what lets the same call serve both sides of the admission
- [x] `load_unit_store` seeds the closure with the `$ref` targets extracted from each candidate **document**, alongside the candidate's `chain_ids()` — a first admission has no stored edges of its own, so the identifier-derived seed is the only thing the closure would otherwise see. An `x-gts-ref` target is not seeded because validating the keyword never reads the target document
- [x] For a revision the roots come from the candidate document's references, never from the previous revision's stored edge set: the overlay wins for edges as it already does for documents. **With one honest limit,** stated at `load_unit_store`: the candidate's *stale* rows are still walked, because `closure` cannot tell a candidate root from any other, so a reference a revision dropped may still be loaded. That is inert — resolution follows the document, and an extra registered schema answers no question — while the direction that is not inert holds: a reference the candidate **added** resolves
- [x] A reference target that genuinely does not exist is distinguishable from a candidate that is not stored yet — `UnitStore::missing_references` beside `missing_candidates`. **No port change was needed.** `closure` already reports every root with no entity row; which of those roots is a candidate is `load_unit_store`'s own knowledge, so the split is one `partition` over the existing result rather than a second list crossing the seam. `missing_candidates` still has no production reader, so T19 inherits both lists and no ambiguity
- [x] The `CLOSURE_BOUND` accounting covers the added roots: `ensure_within_bound` charges `roots.len()` before the first hop, and the seed *is* that vector — reference targets are pushed into it, not passed beside it, so no accounting change was possible to forget

**The dependency reader is `gts-rust`.** `$ref` goes through `gts::extract_gts_refs`, the
canonical definition shared with the resolution that validates the candidate. No local scan
interprets either `$ref` or `x-gts-ref` as a dependency.

**An Instance carries exactly one edge, and that is a rule rather than an omission.** `$ref`
and `x-gts-ref` are schema keywords, so the same strings inside a *value* are data.
Extracting them would invent an edge from a coincidence, and a malformed one would refuse a
valid value — `an_instance_values_ref_shaped_data_is_data_and_not_an_edge` pins it.

**Where the edges travel.** Extracted in `evaluate`, where the document is already parsed and
no transaction is open, and carried on `EvaluatedUnit::edges` as target *identifiers*. The
commit resolves them to rows, because that answer changes between evaluation and commit. Every
edge target must resolve: an absent `$ref` target fails schema resolution, and derivation and
conformance targets are required by the identifier chain. If a target disappears between
evaluation and commit, `DependencyTargetAbsent` fails the unit retryably instead of writing an
incomplete edge set. A malformed `$ref` fails at extraction as `invalid_schema`, the same reason
code `validate_schema` would reach, so no client sees a new outcome.

**Verification:**
- [x] Gear tests, all three backends (see [Commands](#commands)) — 531 on `SQLite`, six consecutive full-suite runs green; `make test-types-registry-db` green on `PostgreSQL` and `MySQL`. No new backend cases: the writes go through `replace_outgoing` and the resolution through `find_by_gts_ids`, both already exercised on both container backends by T4's `closure_walks_a_chain`. T13 adds no new SQL shape
- [x] Table-driven test over edge-kind fixtures, including a derived schema carrying both schema edge kinds, a reference-free schema, and an Instance of a derived type; focused no-edge tests cover `x-gts-ref`
- [x] Test: re-admission removes an edge the new revision dropped — `a_revision_removes_the_edge_it_dropped_and_adds_the_one_it_gained`, with `a_revision_that_keeps_its_reference_keeps_the_edge` for the other direction, because "replace" could otherwise be read as "clear and forget"
- [x] Test: a new schema whose `$ref` names an existing schema **outside its identifier chain** is admitted — the case that failed with `invalid_schema`. It was already pinned, inverted, as `admission_worker_test::a_ref_outside_the_chain_still_fails`; that test is now `a_ref_outside_the_chain_is_admitted` and its flip is the RED→GREEN record. `dependency_test.rs` pins the rows the same admission writes
- [x] Test: a revision that adds one reference and drops another resolves against the new set, not the stored one — `gts_store_test::a_revision_resolves_against_the_reference_it_now_carries`. All three new store tests were verified RED with the one seeding line disabled
- [x] Test: a reference to an identifier no entity carries is reported as a missing reference, distinct from the candidate's own absence — `a_reference_no_entity_carries_is_a_missing_reference_not_a_missing_candidate`, and end to end `an_x_gts_ref_writes_no_row_whether_its_target_exists_or_not` against `a_ref_naming_no_entity_fails_the_candidate`: same absent identifier, two outcomes, because a pattern is satisfiable before anything matches it while a `$ref` is not

**Two existing tests had to be inverted, and both were boundary markers rather than
regressions.** `admission_worker_test::a_ref_outside_the_chain_still_fails` asserted the gap
this task closes. `instance_test::an_instance_admits_against_a_type_from_an_earlier_operation`
asserted an **empty** `dependency` table to prove conformance is identifier-derived; it now
asserts exactly one row, from the Instance to its type, and reads the type's own empty
outgoing set to keep making the original point — the resolution above needed no row.

**SQLite family-lock tests use isolated family keys.** The `SQLite` lock backend is a marker
**file** at `<cache_dir>/cf-gears/locks/{database_scope}/{hash(gear, key)}`
(`toolkit-db/src/advisory_locks.rs`), and `database_scope` for `sqlite::memory:` is identical for
every test in the workspace. So contention is per *lock key*, across processes, however isolated
the databases are — and `nextest` runs each test in its own process. Sibling tests in
`family_test` all admit some version of
`cf.core.example.thing`, they all take that one family key, and the probe carries a 1 ms budget
with no retries.

The lock probe admits `cf.core.lockprobe.thing.v1~`, a family no other test touches. The Instance
and dependency suites likewise own `cf.core.inst.*` and `cf.core.dep.*`. This isolates lock keys
without bypassing the real marker-file lock mechanism. Eight consecutive full-suite runs are
green.

**Dependencies:** Checkpoint 2
**Files touched:** `TR/src/domain/{dependency,dependency_tests}.rs` (new), `TR/src/domain/{mod,enums}.rs`, `TR/src/domain/gts_store.rs`, `TR/src/domain/ports.rs`, `TR/src/domain/admission/{errors,unit}.rs`, `TR/src/api/rest/error.rs`, `TR/src/infra/storage/store.rs`, `TR/tests/dependency_test.rs` (new), `TR/tests/{gts_store_test,admission_worker_test,instance_test,family_test,revision_race_backends_test,common/mod}.rs`, `docs/{PRD,DESIGN}.md`, `docs/database.sql`, and `docs/p0/{SPEC,plan}.md`
**Scope:** M

---

### - [x] T14: Reverse-impact traversal and artifact refresh

**Description:** The reverse-impact read over `dependency` and the refresh of every
affected dependent's effective artifacts in the same transaction as the new revision, with
the fingerprint-stability stop and the `activation_write_set` bound (D5).

**The traversal is one scoped recursive CTE, and the refresh is a loop.** The task was
planned as a worklist for both halves on the premise that a recursive CTE needs raw SQL,
which `11_database_patterns.md` forbids outside migrations. That premise does not hold:
`toolkit-db`'s ADR-0001 (`SecureCteSelect::recursive_cte`) builds a scoped `WITH RECURSIVE`
entirely through `sea-query`, scope embedded in both the seed and the recursive member. So
the traversal became one statement — it runs inside the commit transaction, where round
trips are paid for in lock contention — and only the refresh stayed a loop, because the
fingerprint stop decides the write set by *recomputation*, which no closure query can
express. SPEC §D5 records the split.

**Acceptance criteria:**
- [x] A dependent whose recomputed artifacts are identical is not written and does not move `resource_version` — nor its `revision_no`, nor `updated_at`. Also verified as *examined*: the early-stop test asserts `examined == 2` beside an empty write set, so an empty impact set cannot make it pass
- [x] Every affected dependent's artifacts become current in the same transaction as the new revision — the refresh takes the caller's `&DbTx`, and `commit_revision` calls it after `replace_edges`, so a dependent sees this revision's reference set
- [x] Exceeding `limits.activation_write_set` fails the candidate with a structured reason (`activation_write_set_exceeded`) and commits nothing partial
- [x] `ponytail:` comment names the measured max fan-out (27), the bound (512) and the staging upgrade path — on `DependencyRepo::reverse_impact`
- [x] The traversal terminates on a row that contradicts acyclicity. **The criterion was "terminates on a cyclic graph", and an admitted graph cannot be cyclic** — admission is what keeps it acyclic (ADR-0012, rewritten here with PRD/DESIGN/`database.sql`): derivation strictly shortens the `~`-chain and nothing references an Instance, so those two cannot close a cycle at all, while `$ref` can — alone, or combined with derivation, where a base `$ref`s a schema derived from it. gts-rust refuses the first with `Circular $ref detected`, and T19 refuses both over the combined edge set. The termination property is kept as defence in depth, since a contradicting row would otherwise hang a commit transaction, and the invariant itself is now pinned where it is enforced

**What made a depth-capped CTE safe.** `recursive_cte` requires a `max_depth` and truncates
**silently** past it — a dependent left out keeps stale artifacts marked current, the one
failure this read must not have. The cap is therefore the write-set bound itself, which
makes truncation unreachable below the refusal: seed rows carry depth `0`, so a dependent at
shortest distance `d` appears at depth `d - 1`; a hidden dependent would need a path of at
least `bound + 2` edges, and every one of its `bound + 1` intermediate dependents is nearer
and so already in the set — which puts the set over the bound, where the refusal has already
fired. A returned set is complete; an incomplete walk is an error.

**A refusal after the writes began needed its own channel.** `commit_revision` returns
`Ok(Err(ItemFailure))` for every other candidate refusal, but those are all reached *before*
a write. This one cannot be: the refresh must see the committed revision. Returning it in the
`Ok` position would **commit** the revision it refuses, so it travels as
`WorkerError::RefusedAfterWrite(ItemFailure)` — an error, which rolls the transaction back —
and `process_item` unwraps it into the same `record_failure` path an evaluation-stage refusal
takes. Invisible past the worker; `an_over_bound_write_set_commits_nothing` is what would
have caught the mistake.

**`limits` now reach the worker.** `run_operation` takes `&Limits` and carries it to
`commit_revision`; `activation_write_set` moved off `inert_limit_keys` and gained a
zero-refusal in `validate()`, alongside the other enforced limits. `resolved_document` and
`resolution_closure` are also enforced, per document, during evaluation and refresh. A
refresh refusal uses the same rollback channel as the activation-write-set refusal.

**Two shapes came out of the rebase onto the family lock, not out of T14's design.** `Limits`
and `WorkerSettings` both travel per item, which puts `process_item` one argument over
Clippy's threshold, so T14 groups them as `ItemConfig` and `limits` rides on
`CommitRequest` from there — T15 then replaces that grouping with its own borrowed
`Tuning`, which does the same job for the same reason. And `commit_revision` crossed the
200-line lint once the refresh call joined the tombstone and lock rechecks, so its
`unchanged` branch became `commit_unchanged`: a re-read standing in for the
compare-and-swap a real revision carries in its `WHERE`. T15 had planned that same
extraction for its own clippy bound and inherits it verbatim.

**Verification:**
- [x] Gear tests, all three backends — `cargo nextest run -p cf-gears-types-registry`: **550 passed** (544 before the rebase; the six added are the family-lock suites this branch rebased onto); `--features integration` on `repo_backends_test`, `migration_backends_test` and `revision_race_backends_test`: **6 passed** (PostgreSQL + MySQL containers)
- [x] Test: revising a base with N dependents refreshes exactly N `type_schema` rows — `refresh_test.rs::revising_a_base_refreshes_every_dependent_schema`, over a derived type and an out-of-chain `$ref`erer, asserting the new property reaches the refreshed artifacts
- [x] Test: over-bound case commits nothing — the base's `resource_version`, its whole current row and both dependents' fingerprints are byte-identical after the refusal
- [x] Test: the walk terminates on a cyclic row — `dependency_repo_test.rs` and `repo_test.rs`, both writing the cycle straight through the repository, plus the `PostgreSQL`/`MySQL` case, where a non-terminating backend would hang rather than fail
- [x] Test: acyclicity is enforced, not assumed — `dependency_test.rs::a_revision_that_would_close_a_ref_cycle_is_refused` pins the `invalid_schema` refusal and that no edge is written. The in-batch half (two candidates referencing each other under one overlay) belongs to T19 and is listed there
- [x] `make fmt`, `make clippy` (`--all-targets --features integration`) — clean

**Added:** `TR/tests/dependency_repo_test.rs` (7 tests: transitive reach, root exclusion,
every edge kind, empty set, the bound, no-truncation-past-the-cap, cyclic-row termination),
`TR/tests/refresh_test.rs` (5 tests), `reverse_impact_walks_back_up_a_chain` in
`repo_backends_test.rs`, `a_revision_that_would_close_a_ref_cycle_is_refused` in
`dependency_test.rs`.

**Dependencies:** T13
**Files touched:** `TR/src/infra/storage/repo/{dependency_repo,mod}.rs`,
`TR/src/domain/admission/refresh.rs` (new), `TR/src/domain/admission/{mod,unit,worker,errors}.rs`,
`TR/src/domain/{ports,dependency}.rs`, `TR/src/infra/storage/store.rs`, `TR/src/config.rs`,
`TR/src/api/rest/error.rs`, `TR/src/domain/registry_service.rs`, `TR/tests/*`, plus the
document rewrite: `docs/{PRD,DESIGN,database.sql}`, `docs/ADR/0012-*`, `docs/p0/{SPEC,plan}.md`

**Not `TR/src/domain/dependency/`.** The plan called for splitting that file into
`extraction.rs` + `worklist.rs`. There is no worklist module: the traversal is SQL, and what
remained is an admission step that speaks `ItemFailure` / `WorkerError` and builds a transient
store — `unit.rs`'s neighbours, not extraction's. It went to
`domain/admission/refresh.rs`, and `domain/dependency.rs` stays one pure file.

---

### - [x] T15: Revision-vector guard and bounded retry

**Description:** The multi-pod correctness guard (D4): record a revision vector for every
correctness-relevant dependency and dependent during evaluation, then under the target's
entity lock re-derive the reverse-impact set from the database and compare both membership
and the full vector, rolling back and revalidating within a bounded retry policy.

**Membership is re-derived from the recorded *roots*, not re-read from the recorded rows.**
Comparing versions of the entities evaluation happened to see catches a dependency that
*moved* and misses one that *appeared* — a transitive dependency pulled in because some
intermediate entity gained an edge, and the phantom dependent, which is one of the
criteria. So `RevisionVector` carries the closure roots (the candidate identifier plus its
document's `$ref` targets, a pure function of the authored document) and the commit re-runs
`closure` and `reverse_impact` from them. One derivation function serves both sides, or the
comparison would measure the difference between two readers instead of two states.

**The vector is recorded inside the store-build transaction.** `evaluate` already opens one
`snapshot_read` around `load_unit_store`; the vector's reads join it. Anywhere later and the
comparison would be measuring a gap that opened *before* the evaluation rather than after
it. `UnitStore` grew two accessors for this: `roots()`, because the roots are the closure's
*question* and cannot be recovered from its answer, and `closure_entities()`, because the
vector's dependency half **is** that answer — so evaluation calls `vector::derive_from` with
the rows the store builder already walked, and only the commit side pays for the walk
(`vector::derive`). One function builds the vector on both sides regardless, which is the
property the comparison depends on.

**Step 4.2's row locks over the revision vector are the wrong tool, and the documents now
say so rather than deferring them** (`plan.md` P15; DESIGN §4 corrected, no ceiling). A lock
guarantees only that nothing moves *after* it is taken, and the movement that matters
happens between evaluation and the lock — the phantom dependent appears before any lock
could be held. So the comparison is required either way and the lock is purely additive:
it buys waiting instead of rolling back, which is liveness. It costs one round trip per
vector member inside the commit transaction on a set the bound allows to reach 512, and it
is the only reason step 4.2's canonical ordering had to extend past families at all. What
serializes the rows instead was already there: the candidate's own row by the
compare-and-swap that writes it, and a dependency by the write-write conflict its own
refresh creates on the candidate's `type_schema` row precisely when the move affects it —
and where it does not, the fingerprint-stability stop is the proof the staleness is inert.
That conflict *orders* the two writes and nothing more: a refresh computes its artifacts
before it writes, so the resumed `UPDATE` re-evaluates its predicate, not its payload. The
refresh's write therefore carries the revision and fingerprint its own read saw, and the
loser rolls back rather than overwriting the winner. **Found in review after this task**, and
corrected here rather than left as a claim the code did not make good on. The missing
`FOR UPDATE` is corroboration, not the reason.

**One lock does survive the argument, and it orders commits.** The optimistic mechanism
reaches everything that meets on a row. It does not reach an edge committed *after* a mover's
reverse scan: adding an edge moves no `resource_version` and writes only `dependency`, so the
two commits write no row in common, both pass their own guards, and the dependant keeps an
artifact inlined from a revision that is no longer current with a fingerprint that matches it
— no drift to report and nothing later to repair it. The requirement is a **serialized write path**:
every commit claims the `entity_write_order` row of `types_registry__coordination_state` as its
transaction's first statement.
Either the edge is in `dependency` when the mover's scan runs after that claim, or the unit
writing it has committed nothing and its own guard sees the mover's revision when it does —
two cases, no third. A row rather than an advisory lock, because advisory keys live on a
session separate from the transaction's connection: losing it releases the key while the
transaction carries on, and the exclusion lapses silently.

Consequences for this task: the family lock is no longer taken for a new member only, since
the family advisory locks are gone entirely — the claim makes the transaction exclusive, so
the rules they serialized already are, and keeping them inverted the lock order against
ADR-0013's purge protocol. The vector guard stays load-bearing
for the window the lock does not cover — evaluation runs outside it, so a commit landing
between evaluation and lock acquisition is exactly what
`a_dependency_mutated_between_evaluation_and_commit_costs_one_rollback_and_one_retry` holds a
pass at its evaluation closure read to produce. Every other writer of entity state must take
the lock too, or the order is not total: deletion at T20, the purge job under ADR-0013.
**Found by review, not by a test** — which is the honest provenance;
`a_new_dependant_cannot_commit_while_its_dependency_is_being_revised` is the test it should
have had.

**The `unchanged` branch is not guarded, and that is the decision, not an oversight.** It
writes no revision, moves no version and refreshes no dependent, and the one thing it
decides concerns the current authored document. Guarding it could only turn a
genuine no-op re-submission into a revalidation because a *neighbour* moved, and after
`max_revalidation_attempts` of those, into a failure.

**The retry lives in `process_item`, not in `transaction_with_retry`.** A drift means the
evaluation is void rather than wrong, so what has to be redone is the whole of step 3 — a
fresh snapshot, a fresh transient store, fresh validation. Re-running the same transaction
would compare the same stale vector and drift identically, which is why
`RevalidationRequired` reads as `None` to `retryable_db_err`.

**Early `unchanged` check.** Before evaluation, revision submissions compare the current
canonical authored bytes and content hash in a short read-only snapshot. A match produces
an internal proof carrying the entity ID and expected `resource_version`, without loading
the dependency closure, deriving a vector, validating, or materializing artifacts. Its
commit still claims `entity_write_order` as the first SQL statement, then checks that the
same entity exists, is live, and has the recorded version before CAS-terminalizing the item.
Every authored-content write advances that version; dependency refreshes do not change the
bytes, so their fingerprints need no guard here. A concurrent edit fails the precondition;
a concurrent deletion reports `entity_deleted`. The proof cannot be constructed for a
creation. A probe miss releases its snapshot and runs the ordinary evaluation/commit path;
this adds one small read-only transaction to genuine revisions.

This fixes the case where an identical submission was refused during evaluation because
its dependents exceeded `activation_write_set`. No-op submissions also bypass resolution
and materialization budgets, since they perform neither operation. Real edits retain all
those bounds. Tests cover bounded no-ops, a changed conforming type for an unchanged instance,
and concurrent edits, deletion, and dependency refresh between the probe and commit.

**`limits.activation_write_set` is asked twice for evaluated edits**, because the vector's reverse-impact
read is the refresh's read. An over-bound candidate is refused at *evaluation* under the
same `activation_write_set_exceeded` reason — earlier, cheaper, and invisible to a client;
T14's refusal stays as the backstop for a set that grew in between.

**Acceptance criteria:**
- [x] Vector carries `resource_version` and, where effective content was consumed,
  `resolution_fingerprint` — the latter for live Type Schema dependents, whose effective
  artifacts the refresh consumes to decide whether to rewrite them. `None` for a
  dependency (only its authored document is read, and that moves only with
  `resource_version`), for an Instance dependent (`instance` carries no artifacts) and for
  a tombstone — the two the refresh skips
- [x] A new, removed or moved dependency/dependent rolls the transaction back —
  `VectorDrift::{Appeared, Vanished, Moved, Refreshed}`, travelling as
  `WorkerError::RevalidationRequired`, which is what rolls it back. `Refreshed` is the
  fourth shape and not a redundant one: a refreshed dependent moves no version
- [x] Every artifact write carries a compare-and-swap on `(revision_no,
  resolution_fingerprint)` — `CurrentSchemaCas` — and a miss is
  `VectorDrift::CurrentProjectionMoved`. The two paths differ in what the token is: for the
  **refresh** it is the state its artifacts were computed against, carried out of the read
  that selected the document (`CurrentDocument::projection`); for the **candidate's own
  revision** the artifacts predate any transaction, so its token is read inside the commit and
  is a post-guard sentinel — guard establishes the evaluation still holds, sentinel establishes
  nothing wrote the row since. It is needed because the entity compare-and-swap does not cover
  that row.
  **Found in review, not by a test**: the row lock orders two writes and recomputes neither,
  so the unconditional write let the loser put artifacts from before the winner over it,
  paired with a `revision_no` read afterwards.
  `a_refresh_write_is_a_compare_and_swap_on_the_fingerprint_it_read` pins the predicate.
  **Two interleavings are owed on the container backends** and neither can run on `SQLite`:
  a dependent revising itself under a refresh, and a candidate refreshed by the edge it
  drops. Both were written and both passed unfixed — `SQLite` either refuses the concurrent
  writer with `database is locked` or forces a transaction retry that re-reads the token, so
  the stale-token window never opens. `PausingStores::new_at_occurrence` was added for them
  and is what the container versions need
- [x] Retries are bounded by `worker.max_revalidation_attempts`; exhaustion terminalizes
  the item as `failed` — reason `revalidation_exhausted`, message naming the last drift.
  The key moved off `inert_limit_keys` and gained a zero-refusal in `validate()`, as
  `activation_write_set` did at T14
- [x] ~~Lock order is family → entity/current rows, in canonical identifier order,
  everywhere.~~ **Both halves are retired, for different reasons.** The row half was mistaken:
  a lock over the revision vector cannot do the guard's job and costs a round trip per member
  inside the commit transaction (`plan.md` P15, SPEC §8.1 step 4.2). The family half became
  redundant and then harmful — see the lock paragraph above. What replaced both: the
  `entity_write_order` claim orders commits, and no family lock is taken on either commit
  path — the claim makes the whole transaction exclusive, so the family rules are already
  serialized. Canonical order is kept where it is observable and where locks are actually many:
  the currently uncalled `lock_order` helper sorts and dedups family keys for the next writer,
  and the vector is `(gts_id, role)`-sorted on
  both sides, which is what makes the comparison one merge walk and the reported drift
  deterministic

**Verification:**
- [x] Gear tests, all three backends — `cargo nextest run -p cf-gears-types-registry`:
  **570 passed** (564 before the rebase onto the family lock); `--features integration` on `repo_backends_test`,
  `migration_backends_test` and `revision_race_backends_test`: **6 passed**
  (`PostgreSQL` + `MySQL` containers)
- [x] Test: a dependency mutated between evaluation and commit causes exactly one rollback
  and one successful retry —
  `revalidation_test.rs::a_dependency_mutated_between_evaluation_and_commit_costs_one_\
rollback_and_one_retry`. "One rollback" is read off the versions (revision 2, not 3);
  "successful retry" off the artifacts, which inline the base's **new** property — only a
  fresh evaluation could have produced that, and a committed stale one would still inline
  the old
- [x] Test: a phantom dependent created after the initial scan is detected — and detected on
  *membership*, with no column of any recorded entry changed
- [x] Test: two pods against one database — a commit on one is visible to the other's first
  post-commit read. Two `DBProvider`s with their own pools over one database file; B's
  miss is asserted first, so the hit is a read it actually performed
- [x] Mutation-tested: with both `vector::guard` calls removed, the six guard tests fail and
  the three controls (`a_commit_whose_vector_did_not_move_stands`, the `unchanged`
  re-submission, the two-pod read) still pass — which is the shape that says the suite
  measures the guard and not the fixtures
- [x] `make fmt`, `make clippy` (`--all-targets --features integration`) — clean

**Added:** `TR/src/domain/admission/vector.rs` + `vector_tests.rs` (10 tests over the pure
comparison), `TR/tests/revalidation_test.rs` (9 tests),
`current_projections_read_every_named_entity_that_has_one` in `repo_backends_test.rs`,
`a_zero_revalidation_budget_fails_startup` in `config_test.rs`,
`PausePoint::RevisionEntityRead` in `tests/common/mod.rs`.

**Batched state checks use `current_schema_projections`.** The port returns only
`entity_id`, `revision_no`, and `resolution_fingerprint`; vector derivation and refresh
do not load the three materialized documents merely to compare state. Instance evaluation
uses the same projection to record its conforming type's revision. `current_documents`
uses this narrow SQL projection for its pointer read and selects only identity, authored
text, and content hash from the revision table. `find_current_schema` retains the full
artifacts for entity reads. Transaction boundaries and CAS checks remain unchanged.
`schema_projection_test.rs` checks the executed SQL excludes unused payload columns;
the repository backend suite verifies projection values on PostgreSQL and MySQL.

**Clippy-driven extractions, worth naming because they are also better shapes.**
`process_item` passed the cognitive-complexity bound once the revalidation loop went in, so
the success mapping became the pure `committed_outcome`, and `limits` + `worker` became one
borrowed `Tuning` — which supersedes T14's `ItemConfig`, the same grouping under a name that
also carries the worker tuning. `commit_unchanged` was T15's third such extraction until the
rebase: T14 had already reached the 200-line bound on `commit_revision` and cut it the same
way, so T15 keeps the docstring that states the re-read argument and adds nothing else.

**Dependencies:** T14
**Files touched:** `TR/src/domain/admission/{vector,vector_tests}.rs` (new),
`TR/src/domain/admission/{mod,unit,worker,errors}.rs`, `TR/src/domain/gts_store.rs`,
`TR/src/domain/ports.rs`, `TR/src/domain/registry_service.rs`,
`TR/src/infra/storage/{store.rs,repo/type_schema_repo.rs}`, `TR/src/api/rest/error.rs`,
`TR/src/config.rs`, `TR/tests/*`, `docs/p0/{SPEC,plan}.md`

### - [x] T16: Observability for the admission path

**Description:** Instrument admission so production behaviour is diagnosable: structured
spans per operation and per admission unit, and counters for the outcomes and bounds that
matter.

**Acceptance criteria:**
- [x] One span per operation and one per admission unit, carrying `operation_id`, `gts_id`,
  kind and dry-run mode. `types_registry.admission.operation` opens **before** the first
  read, so it covers the whole pass — which is why its `kind` and `dry_run` are
  `field::Empty` and filled in by `record_operation_facts` once the operation row is
  read. A pass that fails before that read therefore carries *no* `kind` label rather
  than a blank one, which is its own test. `types_registry.admission.unit` restates
  `operation_id`, `kind` and `dry_run` beside `gts_id` and `operation_item_id`:
  deliberate duplication, because under a flat log format a field that lives only on the
  parent span is not on the line an operator greps
- [x] Counters: candidates by terminal status, refusals by reason, revalidation retries,
  activation-set size, worker duration. Five instruments —
  `types_registry_candidates_total{status}`, `types_registry_refusals_total{stage,reason}`,
  `types_registry_revalidations_total{drift}`, `types_registry_activation_write_set` and
  `types_registry_operation_duration_seconds`. **Every label value comes from a closed
  vocabulary** and no identifier is ever a label: the `drift` label is the *shape* of the
  drift (`appeared` / `vanished` / `moved` / `refreshed` / `current_projection_moved` — the
  last one added with the refresh compare-and-swap, and the only shape raised by a write
  rather than by the vector guard), not the `gts_id` that drifted, because that one is
  unbounded and belongs on a span
- [x] Every refusal reason in the acceptance path is countable and distinguishable —
  `AcceptanceError::reason()`, one arm per variant over an exhaustive match, so a refusal
  a later task adds cannot compile until it has a reason — demonstrated by the rebase onto
  the family-lock work, whose `force_compatibility_unavailable` and
  `minor_type_schema_revision` refusals broke the build here until both were named. Distinct from the RFC-9457
  `reason` code `api/rest/error.rs` maps onto, which is deliberately coarse (six variants
  share `VALIDATION_FAILED`): a client branches on the class, an operator needs the
  variant. T17's `Unknown` compatibility verdict is refused in the **worker**, so it
  earns an `ItemFailure` reason and is counted by the same `refusals_total` under
  `stage="admission"` with no change here
- [x] Structured fields only; no print macros (DE13xx) — nothing added uses one

**Verification:**
- [x] Gear tests (see [Commands](#commands)) — `cargo nextest run -p cf-gears-types-registry`:
  **592 passed** (570 before, +22: 12 in-source, 10 integration; 586 before the rebase onto
  the family lock, whose own six suites are the difference).
  `--features integration` on the three container suites: 6 passed
- [x] Test: the instrument contract — rendered names, label keys, label values and bucket
  layouts — asserted against a local `InMemoryMetricExporter`, not reviewed. That is what
  a dashboard depends on, and a dropped `_total` or a renamed label value is invisible in
  a code review and silently empties a panel
- [x] Test: the emission sites, end to end through the real `accept` / `run_operation`.
  `tests/observability_test.rs` installs its own global meter provider and a
  `types_registry=debug` subscriber for the binary, then drives a success, an `unchanged`
  re-submission, an admission refusal, two acceptance refusals, a revision that refreshes
  one dependent, and a real revalidation retry through `PausingStores`
- [x] Mutation-tested: with all eight emission calls and both `.instrument()` layers
  removed, **all 10** integration tests fail; restored, all 10 pass. The suite measures
  the emission and not the fixtures
- [x] `make fmt`, `make clippy` (`--workspace --all-targets --all-features`) — clean
- [x] Manual, at runtime: booted the focused example server
  (`--features static-tenants,static-authn,static-authz,otel`) with the metrics exporter
  pointed at a local OTLP/HTTP sink, then `POST /cf/types-registry/v2/entities` three
  times — an admitted schema (`202`), a revision naming version 1 of an absent identifier
  (`202`, item `failed`), and an empty batch (`400`).
  **Spans:** both appear on the real log lines, nested inside the gateway's own
  `http_request` span —
  `…:types_registry.admission.operation{operation_id=… kind="registration" dry_run=false}:types_registry.admission.unit{operation_id=… gts_id="gts.cf.core.t16.probe.v1~" kind="registration" dry_run=false operation_item_id=1}: … candidate admitted`.
  **Counters:** the pushed OTLP payloads carry
  `types_registry_candidates_total`, `types_registry_refusals_total` and
  `types_registry_operation_duration_seconds`, with the label pairs
  `status=succeeded`, `status=failed`, `stage=acceptance`, `stage=admission`,
  `reason=empty_batch` and `reason=precondition_failed` all present as
  protobuf key/value pairs. Metrics reach a collector only by OTLP push — this gear
  declares no exporter and no `/metrics` endpoint — so the sink is what makes the
  check observable at all
- [x] **One pre-existing boot failure found and set aside, not caused by T16.** With
  `config/quickstart.yaml` unchanged, the focused server dies at types-registry post-init:
  the seeded `…am.tenant_type.v1~cf.core.am.platform.v1~` derives from
  account-management's base schema, which this feature set does not link
  (`Base schema 'gts.cf.core.am.tenant_type.v1~' not found for chain validation`).
  The manual check therefore ran against a copy of the config with `entities: []`.
  Attributed rather than assumed: the failing step is `switch_to_ready`'s chain
  validation on the **legacy in-memory** path, which T16 does not touch — no line of this
  task is on that path — and the missing base schema is a feature-selection fact about the
  focused server, not a registry defect. Not run against `main`'s tree, so "pre-existing"
  here means "independent of this task", which is what the call graph shows

**Two design decisions worth stating, because both are trade-offs:**

**The instruments are a domain port; only the spans are process-global.** *(Record
corrected — `plan.md` P16. This entry and the commit message both argued the opposite, for a
process-global instrument set reached the way `tracing` is. The code that shipped does not do
that, and the port is the shape to extend.)* `domain::ports::metrics::AdmissionMetrics` is the
trait, `infra::metrics` the OpenTelemetry adapter, and `TypesRegistryGear::init` injects the
`Arc<dyn AdmissionMetrics>` into the service, which carries it down
`run_operation` → `process_item` → `commit_evaluated` → the commit transaction's `'static`
closure → `commit_creation` / `commit_revision` → the reverse-impact refresh. The `'static`
closure is why the parameter is an `Arc` and not a reference: each retry attempt clones the
handle. A caller with no meter passes `NoopMetrics`, which is the pre-T16 behaviour exactly, and
that is what the several dozen existing worker test call sites pass. The deciding argument is the
layer boundary: an OpenTelemetry type reached from `domain` through a crate-root module hides
from `de0301_no_infra_in_domain`, and the emission sites are all on the admission call path.
`observability.rs` keeps the two span constructors as free functions, because a `tracing` span
*is* a global sink the emitting code neither carries nor injects, and its module header states
the split. The instrument names carry a configurable prefix (`MetricsConfig`, default
`types_registry`) and the rendered names are pinned by `infra::metrics_tests`.

**The activation-write-set histogram counts revisions, not admissions.** A creation
observes **nothing** rather than zero, because nothing can depend on an identifier the
registry did not hold a moment ago and `commit_creation` runs no reverse-impact refresh at
all. A zero *is* recorded for a revision whose dependents all recomputed to identical
bytes. Both halves are tests, and the creation one carries a control assertion so its zero
reads as scope rather than as silence.

**Two smaller findings, both from the tests:**
- `tests/revalidation_test.rs` needed `#![recursion_limit = "256"]`. Its spawned pass nests
  the whole admission future, and one `tracing::Instrument` layer per level put it over the
  default 128 — the same reason `lib.rs` already carries the attribute
- The integration tests **all** hold one serial lock, including the two span tests. Letting
  the span tests run alongside made `a_creation_observes_no_activation_write_set` see three
  successes instead of one, intermittently: their admissions increment the very counters the
  others measure a delta of. `Temporality::Delta` is what makes the per-test reset
  meaningful — under the default cumulative temporality a flush re-exports every count
  since process start and `reset()` clears the batches without clearing the sums

**Dependencies:** T8 (may run parallel with T14, T15)
**Files touched:**
- `TR/src/observability.rs` — NEW, the two span constructors and the `kind` label
- `TR/src/domain/ports/metrics.rs` — NEW, the `AdmissionMetrics` port and the label vocabularies
- `TR/src/infra/metrics.rs`, `TR/src/infra/metrics_tests.rs` — NEW, the OpenTelemetry adapter and
  the rendered-name contract tests
- `TR/src/observability_tests.rs` — NEW, 12 in-source contract tests
- `TR/tests/observability_test.rs` — NEW, 10 emission tests
- `TR/src/domain/admission/worker.rs` — the two spans, the duration histogram, the
  candidate counters and the revalidation-retry counter; `run_operation` split into a
  wrapper and `run_operation_inner` so the span covers the first read
- `TR/src/domain/admission/acceptance.rs` — `AcceptanceError::reason()`; `accept` split
  into a wrapper and `accept_inner` so every refusal of the path passes one counting point
- `TR/src/domain/admission/unit.rs` — the activation-write-set observation
- `TR/src/gear.rs` — `bind_instruments()` at a known point after `ToolKit` installs the
  provider
- `TR/src/lib.rs`, `TR/Cargo.toml`, `TR/tests/revalidation_test.rs`
**Scope:** S

---

### Checkpoint 3
- [x] Dependent refresh is atomic with the new revision; identical recomputation is a no-op —
  `refresh_test.rs::revising_a_base_refreshes_every_dependent_schema` and
  `a_dependent_whose_artifacts_are_identical_is_not_rewritten`
- [x] Activation bound refuses rather than partially commits —
  `refresh_test.rs::an_over_bound_write_set_commits_nothing`, with the earlier evaluation-side
  refusal from T15 in front of it
- [x] Multi-pod read-after-commit holds —
  `revalidation_test.rs::a_commit_on_one_pod_is_visible_to_the_others_first_read`, two
  `DBProvider`s with their own pools over one database file
- [x] Admission emits spans and metrics — T16, verified at runtime as well as in tests
- [x] `make dylint` — full workspace, once for the phase (P13). Exit 0. Two DE1201 warnings stand,
  both on crates this branch does not touch (`cf-gears-simple-user-settings`,
  `cf-gears-file-storage`): `git log main..HEAD` over their directories is empty, so they are
  pre-existing and not this phase's to clear
- [x] Gear tests at the checkpoint: **617 passed** on SQLite; **6 passed** on the PostgreSQL and
  MySQL container suites, `revision_race_backends_test` included
- [x] **`make test-types-registry-db` did not run `revision_race_backends_test`, and now does.**
  The suite that proves the `entity_write_order` claim on a backend with real row locking was
  reachable only by hand, so `make ci` never ran it — the container test the last commit added to
  close a P0 blocker was outside the gate meant to protect it. One line in the `Makefile` target
- [ ] Human review — the four items checked below, one decision open

**Handoff review (commit `319eb16a5`), item by item.**

1. **The migration and its upgrade test — sound.** `Migrator::up(&db, Some(1))` applies exactly
   one pending migration, so on a fresh database it stops where a deployment that only ran the
   initial migration stops; the test then asserts the table is *absent* before applying the rest,
   which is what makes it an upgrade test rather than a fresh-install one. Nothing else assumes
   the table exists at initial-migration time: `m20260817_000001_initial_tests`'s `P0_TABLES` and
   constraint counts exclude it, and `claim_entity_write_order` fails closed with a message naming
   the migration when the row is missing (`claiming_a_missing_entity_write_order_row_fails_closed`).
   **One gap worth naming:** `the_coordination_state_migration_absorbs_a_table_that_already_exists`
   pins that a pre-created table is left alone — which means such a table keeps whatever shape it
   was created with and never gains `ck_tr_coordination_state_seq`. Correct for the seed, silently
   weaker for the constraint. No path in this repository pre-creates it, so this is a note, not a
   defect.
2. **The claim really is unpreceded, in the code and in the normative text.** `commit_creation`
   (`unit.rs:375`) and `commit_revision` (`unit.rs:745`) both open with
   `claim_entity_write_order`, and the transaction closure in `worker.rs` calls one or the other
   as its only statement — there is no read between `transaction_with_retry` and the claim on
   either branch. SPEC §8.1 step 4.1 says *"nothing may precede it, reads included"* and gives the
   reason; the deletion protocol (§8.1, before Dry Run) repeats it as *"not optional and not
   merely early"*. Both read as instructions T20 and the ADR-0013 purge can follow literally.
   The mechanism is right too: an `UPDATE … SET state_seq = state_seq + 1` holds an exclusive row
   lock to commit, and `#[secure(unrestricted)]` matches every other P0 table.
3. **`ClaimSignallingStores` — the duplication is real and should become a macro, but not here.**
   `tests/common/mod.rs` is 1552 lines, of which roughly 1150 are three decorator stacks —
   `PausingStores`, `ClaimSignallingStores`, `CasMissStores` — each forwarding the same seven port
   traits and each differing in one or two methods. Rust has no trait delegation, so the shape
   that removes it is a declarative `forward_stores!` macro generating the pass-through, with the
   wrapper writing only what it intercepts. T19 and T20 each add interleavings and will each want
   a fourth and fifth wrapper, so the cost compounds. **Decision open:** do it now as a test-only
   refactor, or take the fourth wrapper first and let the macro's shape be argued by three
   examples rather than two.
4. **No stale text survives.** No "wait budget" wording anywhere in the gear. Every remaining
   mention of redelivery either carries the "until T21 … after it" caveat (SPEC lines 528, 594;
   `errors.rs`'s `ConformingTypeAbsent`) or describes ADR-0012's target design, which is where it
   belongs.

---

## Phase 4 — Compatibility

### - [ ] T17: Compatibility against one baseline

**Description:** Baseline selection — the entity's current revision for a major-only
candidate, or the `ACTIVE`/`DELETED` definition of `vM.(n-1)~` for a minor-bearing one —
compared through `GtsStore::compare_documents`, which resolves both sides. `Unknown` is
rejected with its own reason, never collapsed into `Incompatible`.

**Acceptance criteria:**
- [ ] `compare_documents` is the only comparison entry point; `is_minor_compatible` is not used
- [ ] `CompatibilityVerdict::Unknown` fails the candidate with a reason distinct from `Incompatible` (`principle-fail-closed`)
- [ ] Every admitted revision records `gts_spec_version`, `gts_impl_version` and `compat_forced`
- [ ] `force` waives exactly one cross-minor check, only where the deployment enabled it and the candidate has such a check to waive; this removes `ForceCompatibilityUnavailable` and carries the accepted flag into `compat_forced`
- [ ] Major-0 candidates get no baseline and no verdict

**Observability (`plan.md` P16 — this task instruments what it adds):**
- [ ] Every verdict is counted, and `Unknown` is distinguishable from `Incompatible` in the
      metrics and not only in the refusal reason: a new instrument
      `types_registry_compat_verdicts_total{verdict,forced}` over the closed set
      `compatible | incompatible | unknown`. A refusal still increments `refusals_total`; the
      verdict counter exists because `compatible` is not a refusal and has nowhere else to go,
      and because SPEC §16.12's *"rejected with its own reason"* is unobservable when it is one
      `reason` label among a dozen
- [ ] A waived cross-minor check is countable on its own — `forced="true"` on the recorded
      verdict. A `force` is a deployment-enabled policy escape, and `compat_forced` on the row is
      visible only to whoever queries the row
- [ ] The unit span records what the counter cannot: the baseline it selected
      (`baseline_gts_id`, `baseline_revision`), the verdict, and `gts_spec_version` /
      `gts_impl_version`. Identifiers are span fields, never labels
- [x] **The admission reason vocabulary has one home, compile-enforced** (P16 rule 3):
      `ItemFailure::new` takes `AdmissionFailureReason` from `domain::admission::reasons`.
      Stored codes are unchanged; known codes restore typed variants and their metric labels,
      while `Unknown(String)` preserves unfamiliar codes and maps them to `other` in metrics
- [ ] Add this task's compatibility refusal variants to `AdmissionFailureReason`

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Compatibility matrix: optional property added at a `Closed` level (compatible), at `Open` (incompatible), at `Partial` (`Unknown`)
- [ ] Test: provenance columns match `GTS_SPECIFICATION_VERSION` and the crate version
- [ ] Test: `force` refused when `allow_compatibility_force` is off, including on Dry Run
- [ ] Test: the verdict instrument's rendered name, label keys and both label vocabularies
      against an `InMemoryMetricExporter` — T16's bar, and the only thing that catches a dropped
      `_total` or a renamed label value
- [ ] Test: emission end to end through `run_operation` — compatible, incompatible, `Unknown` and
      forced each land under their own label pair — plus a mutation check that removing the
      emission calls fails those tests
- [ ] Test: every `Reason` const is reachable and the vocabulary test enumerates the module, so
      the set a dashboard depends on is asserted rather than greppable

**Dependencies:** Checkpoint 3
**Files likely touched:** `TR/src/domain/compat.rs` (baseline selection; T18's derivation chain joins it and the pair takes `TR/src/domain/compat/` — trigger table above), `TR/src/domain/admission/unit.rs`, `TR/src/domain/error.rs`, `TR/src/domain/admission/reasons.rs` (NEW — the vocabulary), `TR/src/domain/ports/metrics.rs`, `TR/src/infra/metrics.rs`, `TR/src/observability.rs`, `TR/tests/compat_test.rs`
**Scope:** M

---

### - [ ] T18: Derivation chain and major-0 quarantine

**Description:** Identifier-derived chain validation against every managed base, the
Draft-07 dialect pin across a major, and the ADR-0015 quarantine: a stable candidate may not
derive from or `$ref` a major-0 identifier, and a major-0 schema may not carry a registered
Instance. `x-gts-ref` is an instance-value constraint and is outside the quarantine.

**No preflight scan.** ADR-0015 and DESIGN were simplified to drop it (O4): the rule's base case
comes from the release boundary — T2's migration creates the storage in the same release this task
introduces the check — so there is no pre-existing edge to scan for. The obligation that survives
is negative: do not enable the rule against a database populated by a build that had the storage
but not the check. A dev database can be exactly that between T10 and T18; delete it rather than
reasoning about it.

**Acceptance criteria:**
- [ ] Chain bases are reconstructed with `chain_ids()`, not stored or re-derived locally
- [ ] A stable candidate whose immediate base or `$ref` targets include a major-0 identifier is refused. The base comes from `chain_ids()` and `$ref` targets come from `dependency::extract_edges`; no target document is needed to read its major
- [ ] A registered Instance conforming to a major-0 schema is refused, even though the marker is in a preceding segment
- [ ] Dialect is pinned at initial admission and cannot change across revisions of a major

**Observability (P16):**
- [ ] Each quarantine and dialect refusal carries **its own** `Reason` const from T17's
      vocabulary — `stable_derives_from_major_zero`, `stable_refs_major_zero`,
      `instance_of_major_zero`, `dialect_changed` — never collapsed into `invalid_schema`. An
      ADR-0015 refusal and a malformed document are different operator actions, and a shared
      reason makes them one number

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Tests: each quarantine path — a stable candidate deriving from a v0 base and one `$ref`-ing a v0 target; plus stable candidates whose `x-gts-ref` names a v0 entity exactly or through a pattern, which must be admitted because the keyword is outside quarantine
- [ ] Test: dialect change across revisions is refused
- [ ] Test: the four refusals appear in `refusals_total{stage="admission"}` under those exact
      label values — asserted as label values, not as counts

**Dependencies:** T17
**Files likely touched:** `TR/src/domain/derivation.rs` — **second file for the concept, so take `TR/src/domain/compat/`**: `baseline.rs` from T17 plus `derivation.rs` here. Also `TR/src/domain/admission/acceptance.rs`, `TR/tests/quarantine_test.rs`
**Scope:** M

---

### Checkpoint 4
- [ ] Compatibility matrix passes including the `Unknown` tier
- [ ] Provenance persisted on every revision
- [ ] Quarantine and dialect rules hold, including admission of stable schemas whose `x-gts-ref` names a major-0 entity (no preflight — O4)
- [ ] Every verdict is counted, with `Unknown` and a forced waiver each distinguishable in the
      metrics and not only in a refusal reason (T17, P16)
- [ ] Quarantine and dialect refusals each carry their own counted reason, none collapsed into
      `invalid_schema` (T18, P16)
- [ ] Admission reasons live in one compile-enforced vocabulary: a new refusal cannot compile
      without naming one (T17, P16)
- [ ] `make dylint` — full workspace, once for the phase (P13)
- [ ] Human review

---

## Phase 5 — Batching, deletion, dry run, and the REST surface

### - [ ] T19: Dependency-aware partial admission

**Description:** Batch admission over two edge sets, which are not the same set. The
**ordering** graph is authored `$ref`s between candidates, each candidate's
identifier-derived immediate derivation base, its Instance conformance target, and the
implicit `vM.(n-1)~ → vM.n~` edge; the topological sort runs over all of it, because an
Instance must not commit ahead of a Type Schema that may then be refused. The
**cycle-bearing** graph is `$ref` and derivation only — the two an effective form inlines,
so the two a cycle can be built from, and a `$ref`-only check would order a mixed cycle and
admit it. Process in topological order with one candidate per unit, and record an outcome for
every candidate. What ADR-0012 makes acyclic
is the *admitted* relation, not this graph: the overlay makes in-batch candidates visible to
each other, so a batch can author a cycle that nothing has refused yet. The ordering function
therefore detects one and fails its members with `invalid_schema` rather than assuming a
topological order exists. What follows from acyclicity is only what happens after that refusal:
no condensation step and no atomic group. The ordering stays a pure function over a candidate
set.

**Acceptance criteria:**
- [ ] Independent passing branches commit despite failures elsewhere
- [ ] In-batch references resolve against the candidate overlay, never a previously committed revision
- [ ] A failed selected dependency yields `blocked_by_dependency`; a failed lower minor yields `blocked_by_predecessor`
- [ ] A circular `$ref` between two candidates in one batch is refused as `invalid_schema` — the overlay makes both visible to each other, so this is where the acyclicity invariant is actually tested
- [ ] A cycle mixing `$ref` with derivation — a base candidate `$ref`ing a schema derived from it — is refused the same way: the ordering runs over the combined edge set, not over `$ref` alone
- [ ] A candidate with a self-referential GTS `$ref` is refused as `invalid_schema`.
  For self-cycles and multi-candidate cycles, assert every cycle member fails and no new
  entity/revision or outgoing edge is committed; existing revisions remain unchanged
- [ ] The implicit predecessor edge is not written to `dependency`
- [ ] The ordering is exposed as a pure function over a candidate set, usable without a database — required for unit testing without a fixture DB

**Observability (P16):**
- [ ] `blocked_by_dependency` and `blocked_by_predecessor` are `Reason` consts and are counted
      per blocked candidate, so a batch's blocked fan-out is one query rather than a read of
      every item row

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Tests: partial commit, blocked dependent, blocked predecessor, refused in-batch `$ref` cycle
- [ ] Test: batch over `limits.batch_candidates` refused synchronously
- [ ] Test: a batch with one failing dependency emits one `candidates_total{status="failed"}` per
      blocked candidate, under the right `reason` label for each of the two blocking kinds

**Dependencies:** Checkpoint 4
**Files likely touched:** `TR/src/domain/admission/graph.rs`, `TR/src/domain/admission/worker.rs`, `TR/tests/partial_admission_test.rs`
**Scope:** M

---

### - [ ] T20: Deletion and Dry Run

**Inherited from T15 — this is a correctness obligation, not a nicety.** Deletion's *"a new
dependant cannot appear between the deletion check and the lifecycle transition"* rests on the
**`entity_write_order` claim**, not on the optimistic guard: adding an edge moves no `resource_version`
and writes only `dependency`, while deletion writes the target's `entity` row — two different
rows, so nothing serializes them, and the *"no direct registered dependants"* recheck is a
check-then-act on a predicate no compare-and-swap carries.

**Deletion must claim the `entity_write_order` row** as its transaction's first statement, exactly as
admission does, or the order that claim provides stops being total and admission's guarantee
degrades with it. `EntityWriteOrderStore::claim_entity_write_order` is the call;
`EntityWriteOrderStore::claim_entity_write_order` is the whole of it — there is no family half any
more, and T15 retired the advisory locks it would have been.
Locking the revision vector is the wrong tool for a different problem (`plan.md` P15) and is
not what this is. DESIGN §3.7 and SPEC §8.1 step 4.2 state the rule; SPEC §13 carries the
test.

**`EntityRepo::mark_deleted` already exists**, written and unit-tested at T4 with no caller; this is the task that gives it one.

**Description:** The short deletion protocol — positive `expected_resource_version`, family
and entity locks, recheck `ACTIVE` with no direct registered dependents, lifecycle to
`DELETED`, version increment, outcome — and Dry Run as a mode of both registration and
deletion, running every check in a rollback-only transaction.

**Acceptance criteria:**
- [ ] Deletion claims the `entity_write_order` row as its transaction's first statement — without it the commit order admission's correctness rests on is no longer total, and `a_creation_claims_the_entity_write_order_row_exactly_once` (with its revision / `unchanged` siblings) is the shape of the test that catches an omission
- [ ] Deletion with a live direct registered dependent is refused, reporting a count without identities
- [ ] A transitive-only dependent does not block
- [ ] A schema whose `x-gts-ref` names the target does **not** block: the keyword creates no edge, so there is no registered dependent to find
- [ ] Paired public-service deletion scenarios prove the distinction: an otherwise equivalent
  `$ref` holder blocks target deletion, while an `x-gts-ref` holder permits it and stays
  readable. Tenant disable/unavailability scenarios belong to the deferred Availability
  Evaluator, not T20 or P0
- [ ] A deleted entity is still exact-readable as deleted, and absent from lists
- [ ] Dry Run commits nothing, moves no `resource_version`, and its mode is part of the fingerprint
- [ ] Dry-run `succeeded` omits `resource_version`; dry-run `unchanged` reports the existing one

**Observability — this task owns the label sweep (`plan.md` P16 rule 2):**
- [ ] **A dry run is distinguishable from a commit in every series it touches.** `dry_run`
      becomes a label on `candidates_total`, `refusals_total` and T17's verdict counter. Without
      it a rollback-only pass increments `candidates_total{status="succeeded"}` beside admissions
      that actually wrote, and "how many registrations succeeded today" answers with a number
      that includes passes which wrote nothing
- [ ] The activation-write-set histogram is **either labelled or not observed** for a dry run —
      decided here and tested either way, with the reason recorded next to the call. A
      hypothetical write set recorded beside real ones misreports how close the deployment runs
      to `limits.activation_write_set`
- [ ] **A deletion is distinguishable from a registration:** `kind` becomes a label on
      `candidates_total` and `refusals_total`. Deletions are rare and irreversible, and a success
      series that blends them cannot answer *what did this deployment delete*. Both spans already
      carry `kind` and `dry_run` (T16) — the gap is metrics-only, which is why nothing here
      touches a span constructor
- [ ] Both labels are **required parameters** on the port's methods, so every existing call site
      is a compile error until it says which mode and which kind it is. No defaulting to
      `registration` / `false`, which is exactly how a mislabelled series gets shipped
- [ ] Deletion's refusals each carry their own `Reason` const from T17's vocabulary —
      `has_registered_dependents`, `not_active`, beside the existing `precondition_failed`
- [ ] The blocked dependent **count** goes on the unit span (`blocked_dependents`), never in a
      label and never with identities — the same rule the refusal message itself follows

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Tests: blocked deletion, transitive non-blocking, tombstone readability, dry run for both kinds
- [ ] Test: reusing one key for dry run then commit is a fingerprint mismatch, not a replay
- [ ] Test: instrument contract — the two new label keys and their vocabularies against an
      `InMemoryMetricExporter` (T16's bar)
- [ ] Test: emission — a committed deletion, a refused deletion, a dry-run registration and a
      dry-run deletion each land under the right label pair, asserted as a per-pass delta rather
      than a total
- [ ] Test: no counter from a dry-run pass appears under `dry_run="false"`
- [ ] Mutation check: dropping either label, or the deletion emission, fails the suite

**Dependencies:** T19
**Files likely touched:** `TR/src/domain/admission/deletion.rs`, `TR/src/domain/admission/worker.rs`, `TR/src/domain/admission/reasons.rs`, `TR/src/domain/ports/metrics.rs`, `TR/src/infra/metrics.rs`, `TR/src/api/rest/routes.rs`, `TR/tests/deletion_test.rs`
**Scope:** M

---

### - [ ] T27: REST completion, OpenAPI, QUICKSTART

**Moved here from Phase 7 (`plan.md` P17).** Its declared dependencies were T21 and T23 and
neither held: admission already runs inline (`AdmissionMode::Inline` with `NullDispatch`), so
every route here is exercisable without an outbox, and the DTO contract is fixed by SPEC
§10.1/§10.2 rather than by T23's trait. What it does depend on is **T20** — `:batchDelete` and
`DELETE /entities/{entity_key}` are one route pair over the deletion path. Two things move with
it: the changelog entries go to **T24a**, where the break they describe actually happens, and
`make e2e-local` stops being this task's verification and becomes its criterion, because P12
forbids editing an e2e file before T24.

**Description:** The remaining routes — `POST /entities:batchGet`, `POST /entities:batchDelete`,
`DELETE /entities/{entity_key}`, `GET /entities` — plus OpenAPI completeness, the changelog entry for
the `POST /entities` break, and `QUICKSTART.md`.

**Acceptance criteria:**
- [ ] `batchGet` returns one explicit result per requested key, including absence; duplicate keys collapse
- [ ] Every batch item names its entity in one `key` field, classified by the same `EntityKey::parse` the path segment uses; the three batch bodies all name their array `items` (DESIGN §3.3, *Naming a single entity in a batch*)
- [ ] Test: a syntactically impossible identifier answers **identically** through `:batchGet` and through `GET /entities/{entity_key}` — one classifier, so the two surfaces cannot disagree about one string
- [ ] `:batchGet` results echo the `key` they were asked by; `:batchDelete` answers with an operation whose items are keyed by `gts_id`, so a caller that deleted by UUID matches by preserved request order
- [ ] `:batchDelete` items carry a `key` plus a **required positive** `expected_resource_version`; absence is a `400`, not "delete if present"
- [ ] `DELETE /entities/{entity_key}` is a one-item `:batchDelete` over the same domain path — no second deletion model, no handler-local precondition logic. It resolves `{entity_key}` as the `GET` does and requires `Idempotency-Key`
- [ ] Its precondition is a required positive `expected_resource_version` **query parameter**; an `If-Match` header is refused, not ignored. Absent, non-numeric or `0` is a synchronous `400`; a mismatched version is `202` and then `precondition_failed` on the operation item, never `412` (DESIGN §3.3, `DELETE /entities/{entity_key}`)
- [ ] Test: the same mismatched version through `DELETE /entities/{entity_key}` and through a one-item `:batchDelete` yields the identical item outcome — the assertion that keeps the two spellings one model
- [ ] An `If-None-Match` **header** on `:batchGet` is refused, not ignored: validators are per item in `if_none_match`
- [ ] `GET /entities` excludes deleted entities and sorts by canonical identifier
- [ ] `GET /entities` returns **one bounded page and a cursor** (D12): `limit` defaults to `limits.page_size_default` (100) and a request above `limits.page_size_max` (1000) is **refused, not clamped**; cursors come from `toolkit-odata` and an unknown cursor version is rejected rather than reinterpreted
- [ ] The page is **content-free**: the default field set is identity and metadata; all four documents (`content`, `resolved_schema`, `effective_traits`, `effective_traits_schema`) are absent, and a page carries no validator (§8.5)
- [ ] One default set **per surface**: the page is content-free, while `GET /entities/{entity_key}` and `batchGet` return the full representation with D3's artifacts
- [ ] A request carrying **`$select` is refused** with an RFC-9457 problem naming the parameter — never answered with the default representation (§10.2). Accept-and-ignore is wrong here: the caller would get up to 1MB it did not ask for and would build on behaviour P1 changes
- [ ] `EntityRepo::list_page` gets its first real consumer here: its scan budget (`SCAN_BUDGET`, `SCAN_BATCH`) and prefix-range logic (`prefilter_prefix`, `range_upper_bound`) are the most intricate in the layer and have only ever been unit-tested — a test must exercise the budget boundary and a prefix range **through the route**
- [ ] Both read routes go through T4's database read primitives; pattern filtering is `GtsId::matches_pattern` in Rust over prefiltered rows, never SQL that reimplements identifier matching
- [ ] All **seven** routes appear in the OpenAPI document with RFC-9457 error responses registered — the four reads (`GET /entities/{entity_key}`, `GET /entities`, `:batchGet`, `GET /operations/{operation_id}`) and the three mutations (`POST /entities`, `:batchDelete`, `DELETE /entities/{entity_key}`)
- [ ] The three mutation operations are not gateway-published: their operation specs keep
      `exposed = false` until platform identity and a PDP decision are enforced before dispatch
- [ ] `QUICKSTART.md` exists per `02_gear_layout_and_sdk_pattern.md` — description, features, link to `/docs`, one or two working `curl` examples
- [ ] OpenAPI and `QUICKSTART.md` describe this as the platform-plane API for global entities,
      state that mutation routes remain internal-only while platform identity
      (`X-ToolKit-Internal-Token` / `PlatformIdentity`) and the separate listener are unavailable
      (C8), and do not present a gateway mutation `curl` as usable
- [ ] No handler added in this task carries logic the domain service does not already expose — the REST surface stays a mapping layer, so a later gRPC surface cannot diverge from it (SPEC §8.4)
- [ ] Every route is authored on `/v2/` behind the `routes::V2` constant, so T24a's promotion
      stays a constant change and not a sweep (T9a's criterion, inherited)
- [ ] The page and batch bodies follow SPEC §10.1/§10.2 — `items` as the array name, `key` as
      the per-item entity name, `EntityPage` as the page shape — so T23's trait is written
      against that section and not against these handlers (P17)
- [ ] `DELETE /entities/{entity_key}` and `:batchDelete` emit T20's `kind="deletion"` series
      unchanged: they add no second deletion model, so they add no second set of signals
- [ ] **No e2e file is edited by this task** and `make e2e-local` stays green — P12's invariant
      holds until T24 (P17). The v1 routes and the in-memory store they read are untouched
- [ ] The changelog is **not** written here: both breaks land at T24a's promotion, and an entry
      announcing a v1 break in a release where v1 still works is wrong (P17)

**Verification:**
- [ ] Gear tests (see [Commands](#commands)), including `TR/tests/api_rest_test.rs` driven
      through the real router — this is the coverage that replaces `make e2e-local` for the new
      routes
- [ ] `make e2e-local` — **unchanged and still green**, with `git status` showing no e2e file
      touched
- [ ] `make lychee`
- [ ] Manual: `/cf/docs` renders every operation; `curl` against `/v2/` for
      register → poll → read → page → delete, including a `limit` above `page_size_max` and a
      `$select` refusal

**Dependencies:** T20
**Files likely touched:**
- `TR/src/api/rest/routes.rs`
- `TR/src/api/rest/handlers.rs`
- `TR/src/api/rest/dto.rs`
- `TR/tests/api_rest_test.rs`
- `gears/system/types-registry/QUICKSTART.md`
**Scope:** M

---

### Checkpoint 5
- [ ] Partial admission, the refused `$ref` cycle, deletion safety and Dry Run all behave
- [ ] No series blends a dry run with a commit, or a deletion with a registration (T20, P16)
- [ ] Blocked candidates are counted per reason (T19, P16)
- [ ] The REST surface is complete on `/v2/`: all seven routes in OpenAPI, `GET /entities` a
      bounded content-free page whose cursor traverses the set exactly once, `$select` refused,
      `QUICKSTART.md` in place (T27, P17)
- [ ] `make e2e-local` still green with no e2e file edited — P12's invariant holds through this
      phase (T27, P17)
- [ ] Gear tests (see [Commands](#commands))
- [ ] `make dylint` — full workspace, once for the phase (P13)
- [ ] Human review

---

## Phase 6 — Dispatch and the new contract

### - [ ] T21: Outbox dispatch wiring

**Description:** Wire the `toolkit-db` leased outbox with prefix `types_registry_outbox` as
a thin `LeasedMessageHandler` shell over the worker function, mapping its result to
`Ok`/`Retry`/`Reject`. Messages carry only the operation UUID.

**Acceptance criteria:**
- [ ] Handler contains no admission logic — it resolves the operation UUID and calls the worker
- [ ] Delivery is at-least-once and commits are idempotent; duplicate delivery is a no-op
- [ ] Transient database failure returns `Retry`; `Reject` only for a permanently invalid message
- [ ] Candidate content never enters an outbox or dead-letter payload
- [ ] The gear gains the `stateful` capability — SPEC §5 puts it at `[system, db, rest, stateful]` and T2 deferred the fourth to this task
- [ ] **Worker is started at the end of types-registry's `init()`**, not in the stateful `start` (plan decision P3), wired to `ctx.cancellation_token()`; the `OutboxHandle` is retained and `stop()`ed on shutdown
- [ ] Started **after** inline seeding, so seed operations — which are never enqueued — cannot be leased concurrently
- [ ] An operation submitted from any consumer's `init()` is admitted without that consumer waiting for the `start` phase

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Test: duplicate delivery of one operation UUID changes nothing
- [ ] Test: an operation submitted immediately after `init()` returns reaches `completed` without the `start` phase running
- [ ] Test: shutdown drains or cancels cleanly — no task leak after `stop()`
- [ ] Manual: submit over REST against `make example` and observe the operation reach `completed` without direct worker invocation

**Dependencies:** Checkpoint 5
**Files likely touched:** `TR/src/infra/outbox.rs`, `TR/src/gear.rs`, `TR/Cargo.toml`, `TR/tests/outbox_test.rs`
**Scope:** M

---

### - [ ] T22: `toolkit-gts` — `owning_gear` on inventory records

**Description:** Add `owning_gear` to `InventoryTypeSchema` and `InventoryInstance`, derived
at macro-expansion time from the declaring crate's gear name, so the SDK can filter the
process-wide inventory down to what each gear owns (plan decision P4). Without this field
there is no way for a gear to know which inventory records are its own.

**Acceptance criteria:**
- [ ] Both record structs carry `owning_gear: &'static str`
- [ ] `#[gts_type_schema]` and `gts_instance!` populate it without the declaring crate passing anything explicitly
- [ ] `toolkit-gts` exposes a filter — records for one `owning_gear` — beside the existing aggregators
- [ ] Existing aggregators keep working; nothing that reads all records breaks
- [ ] Generated schema documents are byte-identical to before (this change adds metadata, not schema content)

**Verification:**
- [ ] `cargo test -p cf-gears-toolkit-gts`
- [ ] `cargo test --workspace` — every declaring crate still compiles
- [ ] Test: the filter returns exactly one gear's records for a fixture with two declaring crates
- [ ] Manual: diff generated schema documents against the T1 baseline — no change

**Dependencies:** T1 (may run parallel with T21)
**Files likely touched:**
- `libs/toolkit-gts/src/lib.rs`
- `libs/toolkit-gts-macros/src/lib.rs`
- `libs/toolkit-gts/tests/`
**Scope:** M — a library and macro change; blast radius is every declaring crate

---

### - [ ] T23: New SDK trait and the reconciliation helper

**Description:** `TypesRegistryEntities` plus its models per SPEC §10.1, and the
reconciliation workflow of DESIGN §3.3 as an SDK helper so no gear hand-rolls batching,
idempotency or retry (plan decision P4). The old trait is **not** kept — it is deleted in
T26 once every consumer has moved.

**Acceptance criteria:**
- [ ] Trait is object-safe: `hub.get::<dyn TypesRegistryEntities>()` compiles
- [ ] Models are field-for-field the SPEC §10.1 shapes, with out-of-scope fields absent rather than renamed
- [ ] No serde, no utoipa, no HTTP types in the SDK crate
- [ ] Models stay **gRPC-expressible** for future out-of-process use (plan decision P5): flat structures, no `Arc`-linked object graphs, no trait objects. The old trait's `Arc<GtsTypeSchema>` parent chain is exactly what must not be reproduced
- [ ] No security-context parameter; a doc comment records that planes will add one as a deliberate breaking change, and that out-of-process use requires it
- [ ] **Convenience read helpers as provided methods** over the two required primitives, so consumers keep familiar call shapes and the trait stays object-safe (DESIGN: *"single reads and the kind-narrowed `get_type_schema` / `get_instance` are provided methods over it"*): `get_type_schema`, `get_instance`, `get_type_schemas`, `get_instances`, the `_by_uuid` variants, `list_type_schemas`, `list_instances`. Kind narrowing costs no round trip — the kind is the trailing `~` of the identifier, so a kind-mismatched argument fails locally
- [ ] `EntitySnapshot` exposes the materialized documents as **plain fields** (`content`, `resolved_schema`, `effective_traits`, `effective_traits_schema`) plus a `segments` accessor, so the ~40 call sites using the old models' computed methods become field reads rather than rewrites
- [ ] Documents are selectable **individually**, not as an `effective` group: a caller wanting `effective_traits` must not be made to transfer the 1 MB-bounded `resolved_schema` with it (DESIGN §3.3, *Field selection*). `provenance` is the one group that survives
- [ ] **No `effective_*` recomputation exists in the SDK** — the old `GtsTypeSchema::effective_schema` / `effective_properties` / `effective_required` / `effective_traits` / `effective_traits_schema` are not reproduced. They resolved only the parent `$ref` and left non-parent references unresolved, and `effective_traits` was an admitted approximation (`TODO(#1723)`), so reproducing them would reintroduce both a wrong answer and a `constraint-gts-implementation` violation (SPEC §10.1)
- [ ] `EntityQuery` carries `limit` and `cursor`, and `EntityPage` carries the next cursor — the trait already declared `EntityPage` in SPEC §10.1, and without these it is a page in name only (D12)
- [ ] `list_instances` / `list_type_schemas` **hydrate a content-free page through `batchGet`**, so the ~87 existing call sites keep reading payloads from the result. The doc comment states the trade: complete with respect to the traversal, not to an instant, and one extra round trip per page which the client cache absorbs
- [ ] **The validator field is in the models from this task**, and `BatchGet` accepts a validator per requested key in `BatchGetItem::if_none_match`, even though T29 computes them and T30 consumes them. Adding either later would break the SDK contract after ~50 call sites have moved onto it (SPEC §8.5, `plan.md` P9). A result variant for `unchanged` is part of the same shape
- [ ] **Reconciliation helper** implements DESIGN §3.3's five steps: batch-read the desired identifiers, omit content equal to current, set `expected_resource_version` from the read for differing ones and leave it unset for missing ones, return `UpToDate` with no POST when nothing remains, otherwise submit once under one idempotency key and poll to terminality
- [ ] The helper filters the process inventory by `owning_gear` (T22) and batches within `limits.batch_candidates`
- [ ] **Retry lives here, not in gears:** a candidate failing because a dependency is not yet registered is retried a bounded number of times; failure names the gear and identifier
- [ ] One generated idempotency key spans an invocation's retries and polling
- [ ] Callable from a consumer's `init()` (P3). Its doc comment states the one requirement: declare `deps = [types_registry]`, or `init` ordering is not guaranteed

**Verification:**
- [ ] `cargo test -p cf-gears-types-registry-sdk`
- [ ] Test: a mock consumer round-trips submit → poll → read through the new trait
- [ ] Test: helper returns `UpToDate` without submitting when everything already matches
- [ ] Test: helper converges when a base type is admitted only on the second attempt
- [ ] Test: helper against an operation nothing will drain fails on its deadline with a diagnosable error, not a hang

**Dependencies:** T22 (contract shape fixed by SPEC; may start at Checkpoint 4)
**Files likely touched:**
- `TR-SDK/src/entities.rs`
- `TR-SDK/src/entity_models.rs`
- `TR-SDK/src/reconcile.rs`
- `TR-SDK/src/lib.rs`
- `TR/src/domain/local_client.rs`
**Scope:** M

---

### Checkpoint 6
- [ ] An operation submitted through the outbox reaches `completed` with no direct worker call
- [ ] Inventory records carry `owning_gear`; generated schema documents unchanged
- [ ] New trait and reconciliation helper pass against a mock consumer
- [ ] Nothing is cut over yet — consumers still on the old path
- [ ] `make dylint` — full workspace, once for the phase (P13)
- [ ] Human review

---

## Phase 7 — Cutover and migration

### - [ ] T24: Cutover — registry seeds only what it owns; ready mode and in-memory repository out

**Description:** types-registry stops pulling the whole process inventory. It seeds only what
it owns — `toolkit-gts` base types and its own control-plane types — inline at `init()` (P2),
then starts the outbox worker (P3). Delete `switch_to_ready`, the `temporary`/`persistent`
split, `SystemCapability::post_init` and the in-memory repository. From here on reads are
served from the database (SPEC D2, §8.2) — this is the task where the old in-memory read path
is switched off, so it is where that must be true. The `local_client` cache is **not** deleted
(SPEC §8.3, `plan.md` P7): its old model-typed implementation goes when the old models go, and
T30 lands its replacement. Between here and T30 reads are uncached.

**Operator-configured entities (`cfg.entities`).** The YAML `gears.types-registry.config.entities`
field carries operator-controlled identities that cannot be expressed as inventory items — their
GTS identifiers are deployment-specific (e.g. the platform-root and customer tenant types in
`e2e-local.yaml`). Currently these are seeded only into the in-memory `TypesRegistryService`;
T24 must seed them into the database through the same inline admission path used for
types-registry's own inventory. An invalid or oversized `cfg.entities` must fail boot loudly
(current in-memory behaviour preserved). The `cfg.entities` field itself is not removed — it
remains the deployment-time escape hatch for identities that no gear can own.

**Acceptance criteria:**
- [ ] Seeding covers exactly the entities types-registry owns; no other gear's declarations are pulled
- [ ] `cfg.entities` from the deployment configuration is seeded into the database at startup, through the same inline admission path; the field is validated and any failure fails boot
- [ ] Seeding is idempotent — a second start admits nothing new and reports `unchanged` for both owned inventory and `cfg.entities`
- [ ] Seeding runs **before** the outbox worker starts (P3) and enqueues nothing — it invokes the worker inline
- [ ] `init()` never waits on a registrant and never blocks on the outbox (`constraint-boot-path`)
- [ ] Owned inventory and `cfg.entities` together fit within `limits.batch_candidates`; if they exceed it, startup fails loudly rather than silently splitting
- [ ] The v1 REST routes T9a restored are deleted **together with** the repository they read —
      `POST /v1/entities` (`types_registry.register`), `GET /v1/entities/{gts_id}`
      (`types_registry.get`) and the in-memory `GET /v1/entities` list. A route left pointing at a
      deleted repository is the failure mode; T24a then promotes v2 onto those paths
- [ ] `TypesRegistryClient` survives this task over the database, not over the repository it
      deletes: its `register` becomes a submit-then-poll shim for the T24–T26 window, so the
      ~13 `register(...)` sites and every read site keep working while T25/T26 migrate them
      (`plan.md` P17). The shim is one store and one write path — not a dual path — and T26
      deletes it with the trait
- [ ] Ready mode and the in-memory repository are gone; `ready_mode_tests.rs` deleted. The old model-typed cache goes with the old models, and the four `local_client.cache.{type_schemas,instances}.{capacity,ttl}` keys become accepted-and-ignored with a warning naming their T30 replacements
- [ ] `owning_gear` comes from T22's inventory field, not a constant — ceiling C3 is struck from SPEC §9 in this task
- [ ] No entity-derived state survives `init()` — no `ArcSwap`, no entity map, no `GtsOps` field on the gear or the service. Grep-checkable, and the ceilings C1/C4 struck by D2 depend on it

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Test: second `init()` against a populated database seeds nothing
- [ ] Test: the seed set contains no entity owned by another gear
- [ ] Test: `cfg.entities` entries are present and readable after boot, and a second boot reports `unchanged` for them
- [ ] Test: an invalid entry in `cfg.entities` fails boot with a clear error
- [ ] Test: a read issued after an entity is written directly to the database (not through the service) returns it — proving the read path holds no process-local copy. This is the single-process form of SPEC §13's two-pod criterion
- [ ] `make quickstart` — server boots with only registry-owned types present
- [ ] `make e2e-local` — server boots with `cfg.entities` populated (the two AM tenant types); both are readable after boot
- [ ] Manual: restart, confirm entities and artifacts byte-identical

**Dependencies:** T19, T21, T23
**Files likely touched:**
- `TR/src/gear.rs`
- `TR/src/domain/seeding.rs`
- `TR/src/domain/service.rs`
- `TR/src/config.rs` (doc update: `entities` field comment names the inline seeding path)
- `TR/src/infra/storage/in_memory_repo.rs` (deleted); `TR/src/infra/cache/` retyped in T30, not deleted
- `TR/tests/seeding_test.rs`, `TR/tests/ready_mode_tests.rs` (deleted)
**Scope:** M+

---

### - [ ] T24a: Retire v1; promote v2 → v1

**Description:** T24 deletes the in-memory repository, so the v1 routes T9a restored lose the
store they read from and go with it — v1 cannot outlive T24, and repointing it at the database
would be a compatibility shim with no consumer. This task is the other half: every v2 route moves
onto the `/types-registry/v1/` paths, so P0 ends on **one** version rather than a permanent v2.

**Placement.** Directly after T24 (`plan.md` P17). T27 authored the remaining routes
(`:batchGet`, `:batchDelete`, `DELETE /entities/{entity_key}`, the paged content-free
`GET /entities`) back in **Phase 5**, on v2 — so this task promotes all seven routes at once and
nothing is authored on paths that change under it. The Phase 7 order is therefore
**T24 → T24a → T28**, with T25 then T26 alongside it: T25 needs T24, T26 needs T25 — it
deletes the shared trait, so it cannot run beside it. P12's earlier
T24 → T27 → T24a → T28 constraint is gone with the move.

**The e2e window is a consequence of this ordering, not of a defect.** `make e2e-local` goes red
at T24 — where the wire genuinely breaks and where it could not break earlier — and green again
at T28. T25 and T26 sit inside it and are gated by `cargo test --workspace`, `make quickstart`
and `make example` instead. Before T24 the suite stays green, which is what T9a bought and what
T27 preserved by not touching an e2e file.

**Acceptance criteria:**
- [ ] No `/v2/` path remains in the crate, in OpenAPI or in `QUICKSTART.md`
- [ ] The promotion changes paths only: registration and deletion routes remain internal-only
      (`exposed = false`) until C8's platform listener and authorization gate exist
- [ ] `operation_id`s are unchanged by the move — `types_registry.submit_entities`, `.get_operation`, `.get_entity` and T27's additions keep their names, so the promotion is a path change and nothing else. It is still a *breaking* path change for anything on `/v2/`: every such route is removed here, so a caller must move its base path. What the unchanged `operation_id`s buy is that nothing but the path moves — bodies, statuses and semantics are the ones it already had, and the interim surface was never exposed beyond the platform listener
- [ ] All **seven** routes promote together, because all seven exist by Phase 5 (P17)
- [ ] Old v1 handlers, DTOs and routes are **deleted**, not repointed — verified by T24's own criterion that the in-memory repository is gone; `grep -r 'types_registry\.register\|RegisterEntitiesRequest'` finds nothing outside history
- [ ] Every surviving v1 route reads the database; none reads process memory
- [ ] SPEC §10.2 records the final shape and closes the interim window, naming T9a as where it opened and this task as where it closed
- [ ] Changelog: the v1 `POST` break (body shape, `202`, submit-then-poll) and the `GET /entities` shape change are **one release, two entries** — owned here outright, since this is the task where both breaks actually reach a v1 caller (P17)
- [ ] `api_rest_test.rs` needs only its per-version path constant changed — if it needs more, T9a's last criterion was not met and that is the finding, not this task's scope

**Verification:**
- [ ] `cargo test -p cf-gears-types-registry`
- [ ] `make lychee`
- [ ] Manual: `/cf/docs` renders every operation under v1 and no v2 path resolves
- [ ] `make e2e-local` is **expected red** until T28 and green after it; the red set must be exactly the `/entities` call sites T28 owns, and any other failure is a regression this task introduced

**Dependencies:** T24
**Files likely touched:**
- `TR/src/api/rest/routes.rs`, `TR/src/api/rest/handlers.rs`, `TR/src/api/rest/dto.rs`
- `TR/tests/api_rest_test.rs`
- `gears/system/types-registry/QUICKSTART.md`, `CHANGELOG.md`
- `docs/p0/SPEC.md` §10.2
**Scope:** S

---

### - [ ] T25: Migrate system gears and plugins onto the new trait

**Description:** Move every system gear and plugin off `TypesRegistryClient`: reads to the new
trait, and the ~13 explicit `register(...)` sites to the reconciliation helper called from
`init()`. Each gear gates its own readiness on its own registration — DESIGN's *"Each gear
gates only its own readiness"*.

Covers `account-management` (+ static-idp-plugin), `authn-resolver` (+ static and oidc
plugins), `authz-resolver` (+ static and tr plugins), `tenant-resolver` (+ static,
single-tenant and rg plugins), `resource-group`, `usage-collector` (+ plugins), `cluster`,
`credstore` (+ static plugin), `oagw`.

**Acceptance criteria:**
- [ ] No system gear or plugin references `TypesRegistryClient`
- [ ] Read sites move mechanically: T23's provided helpers keep the call shapes, so a read migration is a `use` change plus field reads where a computed method was used
- [ ] Every gear declaring GTS types calls the reconciliation helper once, in `init()`, and declares `deps = [types_registry]`
- [ ] `RegisterResult::ensure_all_ok` sites are replaced by the helper's terminal result — no site treats `pending` as success
- [ ] Registration failure fails that gear's startup naming the gear and identifier, and does not affect other gears

- [ ] Where a materialized `effective_*` field differs from what the deleted client-side method returned, the **materialized value is accepted** — the difference is the old approximation being wrong (unresolved non-parent `$ref`, trait-default order), and `gts-rust` is authoritative. A failing assertion is updated to the new value, never "fixed" back
**Verification:**
- [ ] `cargo test --workspace`
- [ ] `make quickstart` and `make example` — server boots, `/health` green, every migrated gear's types present
- [ ] Test per gear group: registration is idempotent across two starts

**Dependencies:** T24
**Files likely touched:** `gear.rs` and the types-registry call sites of each gear above
**Scope:** L — committed incrementally, one commit per gear; do not batch the whole set

---

### - [ ] T26: Migrate domain gears; delete the old trait

**Description:** The remaining consumers — `bss/ledger`, `bss/rate-provider` (its shared
`registration.rs` helper), `mini-chat` (+ static-audit and static-model-policy plugins),
`llm-gateway`, `model-registry` — then delete `TypesRegistryClient`, its models and
`testing::MockTypesRegistryClient`.

**Acceptance criteria:**
- [ ] No crate in the workspace references `TypesRegistryClient`, `RegisterResult`, `RegisterSummary`, `TypeSchemaQuery` or `InstanceQuery`
- [ ] `rate-provider-sdk`'s shared `register_rate_provider_plugin` uses the new trait, so every rate-provider plugin migrates with it
- [ ] Old trait, its models and its test mock are deleted from the SDK crate
- [ ] `types-registry-sdk` exports only the new surface
- [ ] No consumer recomputes effective artifacts locally — every site reads the materialized group (D3)

- [ ] Where a materialized `effective_*` field differs from what the deleted client-side method returned, the **materialized value is accepted** — the difference is the old approximation being wrong (unresolved non-parent `$ref`, trait-default order), and `gts-rust` is authoritative. A failing assertion is updated to the new value, never "fixed" back
**Verification:**
- [ ] `cargo test --workspace`
- [ ] `grep -r TypesRegistryClient` finds nothing outside history
- [ ] `make example` — boots with every gear's types present

**Dependencies:** T25
**Files likely touched:** the domain-gear call sites above, `TR-SDK/src/api.rs` (deleted), `TR-SDK/src/models.rs`, `TR-SDK/src/testing.rs`
**Scope:** L — split per gear, same rule as T25

---

### - [ ] T28: Update e2e suites for the `202` contract

**Description:** The `POST /entities` break (D10) invalidates every e2e call site that
registers and reads the result synchronously. Those sites move to submit-then-poll: `202`,
then `GET /operations/{id}` until terminal, then assert on the per-candidate outcome.

Surface: `testing/e2e/gears/types_registry/` — six test files, ~95 references to
`/types-registry/v1/entities` — plus `testing/e2e/gears/account_management/conftest.py`,
whose registration helper is setup for another gear's suite.

**Corrected in T9a: the surface is one file wider.** `testing/e2e/gears/oagw/helpers.py:83`
registers a batch of OAGW schemas *and instances* over REST and then reads them back through
`list_oagw_types` (`GET /entities`), so it needs both migrations — submit-then-poll and the
paged list. It was missed because the earlier survey counted `/entities` references only under
`gears/types_registry/` and `account_management/`.

**Acceptance criteria:**
- [ ] A shared polling helper lives in `testing/e2e/gears/types_registry/helpers.py` and is reused; no test open-codes a poll loop
- [ ] The helper has a bounded deadline and fails with the operation's per-candidate errors, never on a bare timeout
- [ ] `account_management`'s registration helper polls to terminality before returning, so that suite's setup stays synchronous from its own point of view
- [ ] Assertions move from the POST body to the operation's per-`gts_id` outcomes
- [ ] `GET /entities` call sites move to the paged, content-free shape (D12): the shared helper pages through the cursor, and any assertion that read `content` from a list result now reads it from `batchGet` or an exact read
- [ ] Tests that assert refusals still assert them **synchronously** — envelope, identifier, policy and idempotency failures stay pre-`202` (SPEC §8.1)

**Verification:**
- [ ] `make e2e-local` — full suite green, including `account_management` and `types_registry`
- [ ] `make e2e-docker`
- [ ] Manual: confirm a rejected candidate surfaces its reason through the polled operation, not as an opaque failure

**Dependencies:** T24a
**Files likely touched:**
- `testing/e2e/gears/types_registry/helpers.py`
- `testing/e2e/gears/types_registry/test_types_registry_{register,get,list,validation,error_handling}.py`
- `testing/e2e/gears/types_registry/test_registration_debug_logging.py`
- `testing/e2e/gears/account_management/conftest.py`
**Scope:** L — split per test file; the `account_management` change is its own commit because it lands in another gear's suite

---

### - [ ] T29: Freshness validators and conditional reads

**Description:** The per-request validator of DESIGN §3.3 and the conditional reads it
enables (SPEC §8.5, `plan.md` P9). For a P0 managed platform-plane read the validator is a
versioned digest over `entity.resource_version`, `type_schema.resolution_fingerprint` (Type
Schemas only) and a default-projection marker — every other input in DESIGN's table is
`tenant plane only`, availability-conditional or external, so none of them applies here.
Ships the `ETag` / `If-None-Match` → `304` path on exact reads and per-key validators on
`batchGet`.

**Acceptance criteria:**
- [ ] Validator is **computed per request, never stored** (`cpt-cf-types-registry-principle-derive-not-store`) — no column, no cache entry holds one as authority
- [ ] Inputs are exactly `resource_version` + `resolution_fingerprint` (Type Schemas; Instances have no derived form) + normalized-projection marker. A `TODO` names the P1 additions: subject visibility-chain version, Context Tenant availability-chain version, routing generation
- [ ] Wire form per DESIGN: base64url of a **versioned** JSON object, identical bytes in `ETag` and in batch bodies; 128-bit digest for the managed case. The version field is what lets P1 add inputs without honouring a P0 token
- [ ] Comparison decodes fields — never compares encoded strings, so serialization differences cannot read as a change
- [ ] Projection is digested as the **normalized field set**, not the query string; absent `$select` equals the explicit default set (RFC 9110 §8.8.3), so a P1 narrow token cannot produce a false `unchanged` for a wider representation
- [ ] Exact read: response carries `ETag`; a matching `If-None-Match` returns a bodyless `304` **that still carries the `ETag`**, declared through `no_content_response(StatusCode::NOT_MODIFIED, ..)`
- [ ] `batchGet`: validators travel **beside individual keys**, in each item's `if_none_match`, because one header cannot represent them; a result may be `unchanged`, and the response stays `200` even when all are. The two body fields are the two header names lowercased — request `if_none_match`, response `etag` — so the batch surface reads like the single one
- [ ] An `unchanged` result **carries its `etag`**, and the exact read's `304` **carries its `ETag`** — RFC 9110 §15.4.5 has a `304` send the validator a `200` would have. Every result but `not_found` therefore has one, so a refresh loop has no special case
- [ ] Test: `unchanged` and `304` both return the same validator the caller sent, byte for byte
- [ ] **Discovery pages carry no validator** and are never conditional (DESIGN: validators are for exact reads, *"never discovery pages"*) — `GET /entities` is unaffected
- [ ] A deleted entity still has a validator, and deletion moves it (deletion increments `resource_version`)
- [ ] Handlers stay mapping-only: the validator is computed in the domain service, so a future gRPC adapter gets it without new domain methods (SPEC §8.4)
- [ ] `ponytail:`-style comment where the digest is built records ceiling C7 — the fixed projection marker and absent chain versions — and names the version field as the upgrade path

**Verification:**
- [ ] Gear tests, all three backends (see [Commands](#commands))
- [ ] Test: unchanged entity yields a byte-identical validator across two reads; a revision changes it
- [ ] Test: a dependent whose `resolved_schema` was refreshed gets a new validator **even when its own `resource_version` did not move** — this is why `resolution_fingerprint` is an input, and it is the case a `resource_version`-only digest gets wrong
- [ ] Test: `If-None-Match` with the current validator returns `304` with no body; with a stale one returns `200` and the document
- [ ] Test: `batchGet` with a mix of current and stale validators returns `200`, `unchanged` for the current ones, full snapshots for the rest
- [ ] Test: an Instance validator omits `resolution_fingerprint` and still changes on revision
- [ ] Test: decoding rejects a validator whose version field is unknown rather than treating it as a match
- [ ] Test: deletion changes the validator

**Dependencies:** T23 (validator field in the models); T27's routes have existed since Phase 5 (P17)
**Files likely touched:**
- `TR/src/domain/validator.rs`
- `TR/src/domain/service.rs`
- `TR/src/api/rest/handlers.rs`, `TR/src/api/rest/routes.rs`
- `TR-SDK/src/entity_models.rs`
- `TR/tests/validator_test.rs`
**Scope:** M

---

### - [ ] T30: SDK client cache — freshness window, byte bound, `fresh` bypass

**Description:** Port the `local_client` cache onto `EntitySnapshot` and give it DESIGN §3.3's
contract (SPEC §8.3, `plan.md` P7). Reads have been uncached since T24; this restores caching,
and because T29 supplies validators the cache **revalidates** rather than merely expiring —
which is what `cpt-cf-types-registry-fr-client-cache` asks for. Deferred to P1 with tenancy:
only the projection / visibility / Context-Tenant key dimensions.

**Acceptance criteria:**
- [ ] Cache is typed on `EntitySnapshot`; the `GtsTypeSchema` / `GtsInstance` implementation is gone with the old models
- [ ] Bound is **bytes** (`store_bound`, default 64MB), LRU-evicted — not an entry count. §3.2 caps one resolved document at 1MB, so the old `capacity: 1024` permitted ~1GB
- [ ] Freshness window (`freshness_window`, default 30s per DESIGN); `0s` is meaningful and disables the window rather than being rejected
- [ ] `fresh` on a read bypasses the window for that call and revalidates unconditionally against the entry's validator
- [ ] **Expiry revalidates, it does not drop:** expired keys and their validators go out in **one** batched conditional `batchGet` — DESIGN's batch poll scheduling — and an `unchanged` result refreshes the confirmation instant while keeping the snapshot. Demand-driven, no timer
- [ ] A failed revalidation **propagates the error and never extends the window** (`principle-fail-closed`); an entry is never served while its revalidation is in flight past the window
- [ ] A terminal successful registration or deletion outcome invalidates every local entry for each returned identifier/UUID pair, under **both** key forms. A `202` acceptance invalidates nothing — the client has not observed the mutation yet
- [ ] Entries are indexed by identifier and by UUID, so either resolution direction hits one snapshot
- [ ] Never cached, each asserted separately: `NotFound`, a failed read, a discovery page or its items, an operation resource
- [ ] The key carries visibility context and projection as fixed P0 markers, so P1 adds dimensions without reshaping it
- [ ] The four old config keys are accepted-and-ignored with a warning naming `freshness_window` / `store_bound`; `ponytail:`-style comment records what is left of ceiling C7 (the key dimensions, not revalidation)

**Verification:**
- [ ] `cargo test -p cf-gears-types-registry`
- [ ] Test: read inside the window after a direct database change returns the cached value; the same read with `fresh` returns the new one
- [ ] Test: `0s` window never serves a cached entry
- [ ] Test: terminal outcome invalidates under identifier **and** UUID; a bare `202` invalidates nothing
- [ ] Test: byte bound evicts on one 1MB document where a thousand small entries do not
- [ ] Test: each of the four never-cached cases
- [ ] Test: a failed read leaves no entry and does not extend an existing window
- [ ] Test: an expired entry whose content did not change is revalidated to `unchanged` and **kept**, not refetched — assert on the absence of a full-snapshot response, not only on the returned value
- [ ] Test: two expired keys produce **one** conditional `batchGet`, not two
- [ ] Test: a failed revalidation surfaces the error and leaves the window unextended
- [ ] `make e2e-local` — no staleness surfaces in the register → poll → read flow

**Dependencies:** T24, T26, T29
**Files likely touched:**
- `TR/src/infra/cache/cache.rs`, `TR/src/infra/cache/cache_tests.rs`
- `TR/src/domain/local_client.rs`
- `TR/src/config.rs`
- `TR/tests/client_cache_test.rs`
**Scope:** M

---

### Checkpoint 7 — ready for review
- [ ] The cutover holds: the real inventory seeds through the new path, every existing consumer works unchanged, the platform boots and stays healthy
- [ ] All 16 success criteria of SPEC §16 met
- [ ] `make ci`, gear tests on three backends, `make e2e-local`, `make e2e-docker`, `make dylint`, `make lychee` green
- [ ] Every ceiling in SPEC §9 has a comment at the point it binds
- [ ] `TypesRegistryClient` is deleted and no crate references it (D6, T26)
- [ ] Conditional reads work end to end: an exact read carries a validator and honours `If-None-Match` with `304`, `batchGet` reports `unchanged` per key (T29)
- [ ] Discovery is bounded: no response is unbounded in items or bytes, and a cursor traverses the whole set exactly once (T27, D12) — proved at Checkpoint 5, re-checked here on the promoted v1 paths
- [ ] The client cache is in place on the new models with its window, byte bound, `fresh` bypass and batched conditional revalidation (T30) — P0 does not ship an uncached read path
- [ ] Human review
