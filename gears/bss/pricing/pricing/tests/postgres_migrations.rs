//! Postgres-only: the migration chain, executed against the backend it targets
//! and **through the runner production uses**.
//!
//! Until this suite existed the chain had never run on Postgres. Every trigger,
//! CHECK and partial index was verified by reading the statement text beside the
//! executed `SQLite` mirror — which proves the two branches say the same thing
//! and proves nothing about whether either is accepted by the server.
//!
//! # Two things this suite is careful about, both learned the hard way
//!
//! **It runs the chain the way the gear boots.** `DatabaseCapability` hands
//! `Migrator::migrations()` to the toolkit runner
//! ([`run_migrations_for_testing`] is its test entry point), which applies in
//! **name** order and books into an *unqualified* `toolkit_migrations_*` table.
//! `MigratorTrait::up` does neither — it applies in vec order and books into
//! `seaql_migrations` — so a suite built on it exercises an ordering production
//! never uses and cannot see the C1 bug class the sibling ledger found: an
//! unqualified bookkeeping table resolving into whichever schema the
//! connection's `search_path` puts first, so that boot 2 finds an empty history
//! and re-runs every migration into a crash loop.
//!
//! **It pins rosters by name, not by count.** A count is satisfied by *any* set
//! of the right size, so a constraint replaced by `CHECK (1 = 1)` keeps a count
//! green — which is exactly how fourteen `pricing_price` CHECKs once could each
//! have been neutralised with the whole suite green. Every CHECK in this chain is
//! uniquely named, so the roster was free and the count was a false economy.
//!
//! This paragraph said "any **62** objects" until 2026-08-04, and by then the
//! roster held 76. That is the small, characteristic way a count rots and a
//! roster does not: the assertion stayed correct because it names its members,
//! while the sentence explaining the assertion carried a number nobody was
//! obliged to update. The number is gone rather than corrected — the rosters
//! below are the count, and a second copy of it in prose is a second thing to
//! keep true.
//!
//! # What a Postgres suite has to do to be evidence
//!
//! **Prove a constraint by executing the statement it must refuse**, and assert
//! the error names that constraint. A test that writes only valid values catches
//! a constraint that got *narrower* and never one that stopped refusing.
//!
//! **Put the world in the state where the object under test is what answers.** A
//! refusal an earlier guard produces is not evidence about the guard named in the
//! test.
//!
//! **Every guard must be provable by removal**: delete the `CONSTRAINT` or the
//! trigger, watch *exactly one* test fail, restore, and report which test it was.
//!
//! **The rosters issue no DML and are therefore evidence of presence only.** That
//! the objects reached the server is what they say; that any of them *refuses*
//! what it claims to is Track P's, one executed refusal per object.
//!
//! The one exception is `a_key_widening_applies_over_rows_the_table_already_holds`,
//! and it is an exception about the **runner** rather than about a guard: a
//! migration that alters a key has to be applied to a table that already holds
//! rows, which is a state no boot of an empty chain reaches, so that case writes
//! rows between two staged runs. It is here and not in a schema suite because what
//! it exercises is the chain being applied in two halves.
//!
//! Ignored by default — they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use bss_pricing::infra::storage::migrations::Migrator;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, connect_db};

/// Every migration the gear boots with, coord's spliced lease table included.
///
/// **Derived, never written down.** It was a literal `17`, and the literal went
/// stale the day the chain gained `pricing_approval_key`: two boot tests then failed
/// on a count nobody had changed on purpose, which is the same "a count beside a
/// roster, and only one stays true" failure the register migration's own module doc
/// records — here with the roster in code rather than in prose. `Migrator::migrations()`
/// is the roster, so the count comes off it.
///
/// It is more than the `pricing_*` files: the list also carries
/// `coord::migration::Migration::in_schema("bss")`, whose `m0001_…` name sorts
/// **first** under the runner's name ordering and therefore runs before anything
/// in this gear. That is why the number is taken from the list the runner is handed
/// and not from a directory listing.
fn chain_len() -> usize {
    Migrator::migrations().len()
}

/// Below the workspace floor (`test_containers::POSTGRES_TAG`) on purpose,
/// matching `pg_support::PG_TAG`; see its note on why `postgres_tagged` and not
/// the bare default. `GEARS_TEST_PG_TAG` still overrides it.
const PG_TAG: &str = "16-alpine";

/// A running Postgres, its port, and the container guard.
///
/// The guard is returned because dropping it stops the container: a caller that
/// bound only the port would race its own database to the end of the test.
async fn pg() -> (u16, ContainerAsync<Postgres>) {
    let container = test_containers::postgres_tagged(PG_TAG)
        .start()
        .await
        .expect("start postgres");
    let port = host_port(&container).await;
    (port, container)
}

/// The published host port for the container's 5432, waited for rather than read
/// once.
///
/// # Why this is a loop, measured rather than assumed
///
/// This suite failed intermittently with
/// `PortNotExposed { port: Tcp(5432) }` — 1–3 of the 11 tests per run, a different
/// member each time, which is what made it look like contention over the shared
/// harness the sibling suites use. It is not: these tests each start their own
/// container, and the failure was diagnosed by printing the container's own log at
/// the moment of the error. The log says the container is **healthy**:
///
/// ```text
/// LOG:  listening on IPv4 address "0.0.0.0", port 5432
/// LOG:  database system is ready to accept connections
/// ```
///
/// No crash and no exit. So the container is up and the *port map* is what is
/// missing: `get_host_port_ipv4` performs a live `inspect` (`RawContainer::ports`
/// → `docker_client.ports`, no cache), and under load Docker answers with
/// `NetworkSettings.Ports` not yet carrying the binding it is about to publish.
/// Reading once is reading too early.
///
/// The first guess — that `PortNotExposed` meant the container had stopped, Docker
/// dropping a dead container's bindings — was wrong, and only the container log
/// ruled it out. That is why the log stays in the failure path below: without it
/// this reads as a crash and the retry looks like papering over one.
async fn host_port(container: &ContainerAsync<Postgres>) -> u16 {
    /// Long enough to cover the observed gap by two orders of magnitude, short
    /// enough that a genuinely unexposed port still fails inside a test timeout.
    const ATTEMPTS: u32 = 40;
    const GAP: Duration = Duration::from_millis(50);

    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match container.get_host_port_ipv4(5432).await {
            Ok(port) => return port,
            Err(cause) => {
                last = Some(cause);
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(GAP).await;
                }
            }
        }
    }

    let out = container.stdout_to_vec().await.unwrap_or_default();
    let err = container.stderr_to_vec().await.unwrap_or_default();
    panic!(
        "map the postgres port after {ATTEMPTS} attempts over {:?}: {}\n\
         --- container stdout ---\n{}\n--- container stderr ---\n{}",
        GAP * ATTEMPTS,
        last.expect("the loop ran at least once and every arm records its error"),
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
}

/// A DSN carrying `search_path` as a libpq option, the way the gear's runtime
/// config sets it per connection.
fn url_with_search_path(port: u16, search_path: &str) -> String {
    format!(
        "postgres://postgres:postgres@127.0.0.1:{port}/postgres?options=-c%20search_path%3D{search_path}"
    )
}

fn plain_url(port: u16) -> String {
    format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres")
}

async fn count(conn: &DatabaseConnection, sql: &str) -> i64 {
    let row = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await
        .expect("run the catalog query")
        .expect("the catalog query must return a row");
    row.try_get::<i64>("", "n").expect("read the count")
}

/// `EXPLAIN`'s output, joined. Its column is always named `QUERY PLAN`, so
/// [`names`] cannot read it and an `AS v` inside the explained statement renames
/// the *inner* projection rather than `EXPLAIN`'s own.
async fn explain(conn: &DatabaseConnection, sql: &str) -> String {
    conn.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("EXPLAIN {sql}"),
    ))
    .await
    .expect("run the explain")
    .iter()
    .map(|row| {
        row.try_get::<String>("", "QUERY PLAN")
            .expect("read the plan line")
    })
    .collect::<Vec<_>>()
    .join("\n")
}

/// One `v` column of a catalog query, in the order the query asked for.
async fn names(conn: &DatabaseConnection, sql: &str) -> Vec<String> {
    conn.query_all_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .expect("run the catalog query")
    .iter()
    .map(|row| row.try_get::<String>("", "v").expect("read the name"))
    .collect()
}

const FUNCTIONS_SQL: &str = "SELECT p.proname AS v FROM pg_proc p \
     JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = 'bss' ORDER BY 1";
const TRIGGERS_SQL: &str = "SELECT t.tgname AS v FROM pg_trigger t \
     JOIN pg_class c ON c.oid = t.tgrelid \
     JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = 'bss' AND NOT t.tgisinternal ORDER BY 1";
const CHECKS_SQL: &str = "SELECT co.conname AS v FROM pg_constraint co \
     JOIN pg_namespace n ON n.oid = co.connamespace \
     WHERE n.nspname = 'bss' AND co.contype = 'c' ORDER BY 1";
const PARTIAL_INDEXES_SQL: &str = "SELECT indexname AS v FROM pg_indexes \
     WHERE schemaname = 'bss' AND indexdef LIKE '%WHERE%' ORDER BY 1";
/// Every **relational** constraint of the chain — foreign keys, table-level
/// `UNIQUE`s and `EXCLUDE`s — as `name: definition`.
///
/// `CHECKS_SQL` above selects `contype = 'c'` and `PRIMARY_KEYS_SQL` selects
/// `'p'`; until this roster existed `'f'`, `'u'` and `'x'` were selected by
/// nothing, so a foreign key dropped from a migration's Postgres arm reached the
/// server missing and no assertion on either tier said so. **Refusal cannot
/// stand in for it**: this shard documents twice that a `BEFORE` trigger answers
/// ahead of constraint checking, so most child keys here can never *be* the
/// object that refuses (`postgres_schema_bundle.rs` for
/// `fk_pricing_bundle_component_bundle`, `postgres_schema_composite_meter.rs`
/// for `fk_pricing_composite_meter_revision`, and
/// `postgres_schema_plan_shape.rs::a_bound_cannot_hang_off_a_revision_that_does_not_exist`
/// which reads the definition out of the catalog for exactly this reason).
///
/// The **definition** is rostered, not just the name, because that is the half a
/// refusal could not show either: a composite key rebuilt over one column, or
/// re-pointed at a different parent, keeps its name.
const RELATIONAL_CONSTRAINTS_SQL: &str = "SELECT co.conname || ': ' \
     || pg_get_constraintdef(co.oid) AS v FROM pg_constraint co \
     JOIN pg_namespace n ON n.oid = co.connamespace \
     WHERE n.nspname = 'bss' AND co.contype IN ('f', 'u', 'x') ORDER BY 1";
/// Every index this chain writes a `CREATE INDEX` for (Z6-5).
///
/// The `NOT EXISTS` is what makes the set comparable to the `SQLite` roster: a
/// primary key's backing index is created *by the constraint*, named by the server
/// and rostered by `PRIMARY_KEYS_SQL`, so counting it here would compare a
/// migration's declarations against the server's bookkeeping.
const INDEXES_SQL: &str = "SELECT i.indexname AS v FROM pg_indexes i \
     JOIN pg_class c ON c.relname = i.indexname \
     JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = i.schemaname \
     WHERE i.schemaname = 'bss' AND c.relkind = 'i' \
     AND NOT EXISTS (SELECT 1 FROM pg_constraint co WHERE co.conindid = c.oid) \
     ORDER BY 1";
/// The chain's foreign keys, table `UNIQUE`s and `EXCLUDE`s, by name **and
/// definition** — see [`RELATIONAL_CONSTRAINTS_SQL`] for why the definition is
/// in the roster.
const EXPECTED_RELATIONAL_CONSTRAINTS: &[&str] = &[
    // D-09's non-overlap invariant on the membership plane, and its sibling on
    // the window plane (`pricing_price_window`, the read two writers could step
    // through). Both are `contype = 'x'`; only the second is partial, which is
    // why only the second appears in `EXPECTED_PARTIAL_INDEXES`.
    "excl_pricing_group_membership_no_overlap: EXCLUDE USING gist (tenant_id WITH =, \
     payer_tenant_id WITH =, tstzrange(effective_from, effective_to, '[)'::text) WITH &&)",
    "excl_pricing_price_window_no_overlap: EXCLUDE USING gist (tenant_id WITH =, \
     price_id WITH =, tstzrange(effective_from, effective_to, '[)'::text) WITH &&) \
     WHERE ((state = ANY (ARRAY['scheduled'::text, 'active'::text])))",
    "fk_pricing_bulk_row_lock_operation: FOREIGN KEY (bulk_operation_id) \
     REFERENCES bss.pricing_bulk_operation(operation_id)",
    "fk_pricing_bulk_row_lock_price: FOREIGN KEY (price_id) \
     REFERENCES bss.pricing_price(price_id)",
    "fk_pricing_bundle_component_bundle: FOREIGN KEY (bundle_id) \
     REFERENCES bss.pricing_bundle(bundle_id)",
    "fk_pricing_bundle_revshare_group: FOREIGN KEY (bundle_id, plan_revision, vendor_sku_id) \
     REFERENCES bss.pricing_bundle_revshare_group(bundle_id, plan_revision, vendor_sku_id)",
    "fk_pricing_bundle_revshare_group_bundle: FOREIGN KEY (bundle_id) \
     REFERENCES bss.pricing_bundle(bundle_id)",
    // The five composite keys onto `pricing_plan (plan_id, revision)`. Each is
    // the half `postgres_schema_plan_shape.rs` says a refusal cannot show: a
    // single-column key would refuse the same row and would let a child sit
    // under a revision number its parent never had.
    "fk_pricing_composite_meter_revision: FOREIGN KEY (plan_id, plan_revision) \
     REFERENCES bss.pricing_plan(plan_id, revision)",
    "fk_pricing_plan_addon_rule_revision: FOREIGN KEY (plan_id, plan_revision) \
     REFERENCES bss.pricing_plan(plan_id, revision)",
    "fk_pricing_plan_descriptor_set_revision: FOREIGN KEY (plan_id, plan_revision) \
     REFERENCES bss.pricing_plan(plan_id, revision)",
    "fk_pricing_plan_period_floor_cap_revision: FOREIGN KEY (plan_id, plan_revision) \
     REFERENCES bss.pricing_plan(plan_id, revision)",
    "fk_pricing_plan_phase_revision: FOREIGN KEY (plan_id, plan_revision) \
     REFERENCES bss.pricing_plan(plan_id, revision)",
    "fk_pricing_price_overlay_line_amount_line: FOREIGN KEY (tenant_id, overlay_revision, line_id) \
     REFERENCES bss.pricing_price_overlay_line(tenant_id, overlay_revision, line_id)",
    "fk_pricing_price_overlay_line_overlay: FOREIGN KEY (price_overlay_id, overlay_revision) \
     REFERENCES bss.pricing_price_overlay(price_overlay_id, revision)",
    "fk_pricing_price_tier_band_price: FOREIGN KEY (price_id) \
     REFERENCES bss.pricing_price(price_id)",
    "fk_pricing_price_window_price: FOREIGN KEY (price_id) \
     REFERENCES bss.pricing_price(price_id)",
    "fk_pricing_repricing_journal_applied_price: FOREIGN KEY (applied_price_id) \
     REFERENCES bss.pricing_price(price_id)",
    "fk_pricing_repricing_journal_price: FOREIGN KEY (price_id) \
     REFERENCES bss.pricing_price(price_id)",
    "fk_pricing_repricing_journal_run: FOREIGN KEY (run_id) \
     REFERENCES bss.pricing_bulk_operation(operation_id)",
    // The one table-level `UNIQUE` (`contype = 'u'`) the chain declares: every
    // other uniqueness in it is a partial `CREATE UNIQUE INDEX`, which is why
    // this list is short and `EXPECTED_INDEXES` is not.
    "uq_pricing_price_tier_band_lower_bound: UNIQUE (price_id, from_qty)",
];

/// Every table's primary key as `table: col, col` (D-236).
///
/// `unnest(conkey) WITH ORDINALITY` is what makes this a mirror of the `SQLite`
/// side rather than a near-miss: `conkey` is the key's **own** column order, and
/// aggregating without it would sort by `attnum` — the order the columns were
/// declared in — so a composite key rearranged into a different key would read
/// identical on both engines.
const PRIMARY_KEYS_SQL: &str = "SELECT c.relname || ': ' \
     || string_agg(a.attname, ', ' ORDER BY k.ord) AS v \
     FROM pg_constraint co \
     JOIN pg_class c ON c.oid = co.conrelid \
     JOIN pg_namespace n ON n.oid = c.relnamespace \
     CROSS JOIN LATERAL unnest(co.conkey) WITH ORDINALITY AS k(attnum, ord) \
     JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
     WHERE n.nspname = 'bss' AND co.contype = 'p' \
     GROUP BY c.relname ORDER BY c.relname";

/// The PL/pgSQL functions the chain declares — the objects the `SQLite` mirror
/// cannot carry at all, since it has no procedural language.
///
/// Fewer than the mirror's triggers, and deliberately: `SQLite` needs several
/// literal-message triggers where Postgres needs one function interpolating
/// `TG_OP`. **No count is written here.** It said "eleven" over a roster of
/// thirteen and "forty-seven `CREATE TRIGGER` statements" over a chain that has
/// more than that — a number in prose beside a roster in code is one of them
/// wrong the first time either grows, and the roster is the one the assertion
/// reads.
const EXPECTED_FUNCTIONS: &[&str] = &[
    "pricing_approval_append_only",
    "pricing_approval_key_append_only",
    "pricing_approval_key_follow_state",
    "pricing_approval_threshold_no_delete",
    "pricing_approval_threshold_no_update",
    "pricing_approval_threshold_tombstone_no_delete",
    "pricing_approval_threshold_tombstone_no_update",
    "pricing_audit_log_append_only",
    // Slice 12: one PL/pgSQL function, three SQLite triggers.
    "pricing_bulk_operation_transitions",
    "pricing_bulk_row_lock_custody",
    "pricing_bundle_component_append_only",
    "pricing_bundle_revshare_append_only",
    "pricing_bundle_revshare_group_append_only",
    // Slice 10's composite meter: one PL/pgSQL function, three SQLite triggers.
    "pricing_composite_meter_append_only",
    // Slice 11. One PL/pgSQL function carrying the five arms the SQLite mirror
    // spells as five triggers.
    "pricing_migration_append_only",
    "pricing_plan_addon_rule_append_only",
    "pricing_plan_append_only",
    "pricing_plan_descriptor_set_append_only",
    "pricing_plan_period_floor_cap_append_only",
    "pricing_plan_phase_append_only",
    "pricing_price_append_only",
    "pricing_price_overlay_append_only",
    "pricing_price_overlay_line_amount_append_only",
    "pricing_price_overlay_line_append_only",
    "pricing_price_tier_band_append_only",
    "pricing_price_tier_band_kind",
    "pricing_price_tier_band_parent_kind",
    "pricing_price_window_append_only",
    // Slice 12: one PL/pgSQL function, four SQLite triggers.
    "pricing_repricing_journal_progress",
    // Slice 11. Two unconditional arms in one function: a migrated-origin
    // snapshot is frozen, so no UPDATE is sanctioned at all.
    "pricing_snapshot_provenance_frozen",
];

/// The triggers those functions are bound to, one per function.
const EXPECTED_TRIGGERS: &[&str] = &[
    "trg_pricing_approval_append_only",
    "trg_pricing_approval_key_append_only",
    "trg_pricing_approval_key_follow_state",
    "trg_pricing_approval_threshold_no_delete",
    "trg_pricing_approval_threshold_no_update",
    "trg_pricing_approval_threshold_tombstone_no_delete",
    "trg_pricing_approval_threshold_tombstone_no_update",
    "trg_pricing_audit_log_append_only",
    "trg_pricing_bulk_operation_transitions",
    "trg_pricing_bulk_row_lock_custody",
    "trg_pricing_bundle_component_append_only",
    "trg_pricing_bundle_revshare_append_only",
    "trg_pricing_bundle_revshare_group_append_only",
    "trg_pricing_composite_meter_append_only",
    "trg_pricing_migration_append_only",
    "trg_pricing_plan_addon_rule_append_only",
    "trg_pricing_plan_append_only",
    "trg_pricing_plan_descriptor_set_append_only",
    "trg_pricing_plan_period_floor_cap_append_only",
    "trg_pricing_plan_phase_append_only",
    "trg_pricing_price_append_only",
    "trg_pricing_price_overlay_append_only",
    "trg_pricing_price_overlay_line_amount_append_only",
    "trg_pricing_price_overlay_line_append_only",
    "trg_pricing_price_tier_band_append_only",
    "trg_pricing_price_tier_band_kind",
    "trg_pricing_price_tier_band_parent_kind",
    "trg_pricing_price_window_append_only",
    "trg_pricing_repricing_journal_progress",
    "trg_pricing_snapshot_provenance_frozen",
];

/// The partial indexes — the `WHERE`-carrying ones, where the predicate *is* the
/// rule (one current revision per plan, one open draft, one terminal phase).
/// Every column in the chain whose name says it holds a revision, as
/// `table.column type` (Z6-7).
///
/// Matched on the name because that is what a reader matches on: a column called
/// `…_revision` that is not `bigint` is the outlier this exists to find, whichever
/// table grows it next. `plan_revision`, `subject_revision`, `source_revision` and
/// the bare `revision` all end in the same eight characters.
const REVISION_COLUMNS_SQL: &str = "SELECT table_name || '.' || column_name || ' ' || data_type \
     AS v FROM information_schema.columns \
     WHERE table_schema = 'bss' AND column_name LIKE '%revision' ORDER BY 1";
/// Every column of the chain whose name ends `revision`, with the type it carries.
///
/// A **roster and not a floor.** A floor of twelve over a population of fifteen
/// holds with three of them gone and cannot say which: it stays green all the way
/// down to the number it names, however much larger the population really is. A
/// named member is either extracted or it is not.
const EXPECTED_REVISION_COLUMNS: &[&str] = &[
    "pricing_bundle_component.plan_revision bigint",
    "pricing_bundle_revshare.plan_revision bigint",
    "pricing_bundle_revshare_group.plan_revision bigint",
    "pricing_catalog_version_ref.subject_revision bigint",
    "pricing_composite_meter.plan_revision bigint",
    "pricing_migration.source_revision bigint",
    "pricing_plan.revision bigint",
    "pricing_plan_addon_rule.plan_revision bigint",
    "pricing_plan_descriptor_set.plan_revision bigint",
    "pricing_plan_period_floor_cap.plan_revision bigint",
    "pricing_plan_phase.plan_revision bigint",
    "pricing_price_overlay.revision bigint",
    "pricing_price_overlay_line.overlay_revision bigint",
    "pricing_price_overlay_line_amount.overlay_revision bigint",
    "pricing_snapshot_provenance.source_revision bigint",
];

const EXPECTED_PARTIAL_INDEXES: &[&str] = &[
    // Not a `CREATE INDEX` of this chain but the index Postgres builds for
    // `excl_pricing_price_window_no_overlap`'s `EXCLUDE` constraint, and it is *partial* — the
    // constraint carries `WHERE state IN ('scheduled','active')`, so
    // `PARTIAL_INDEXES_SQL`'s `indexdef LIKE '%WHERE%'` selects it. Its sibling
    // `excl_pricing_group_membership_no_overlap` has no predicate and therefore
    // is not here; both are rostered as constraints by
    // `EXPECTED_RELATIONAL_CONSTRAINTS` below.
    "excl_pricing_price_window_no_overlap",
    "idx_pricing_outbox_undrained",
    "idx_pricing_price_supersedes",
    "uq_pricing_approval_key_pending",
    "uq_pricing_approval_policy_pending",
    "uq_pricing_plan_current",
    "uq_pricing_plan_open_draft",
    "uq_pricing_plan_phase_terminal",
    "uq_pricing_price_meter_line_current",
    // D-107. Without the predicate a draft revision of a published overlay
    // collides with itself and an overlay is authorable exactly once.
    "uq_pricing_price_overlay_open_draft",
    "uq_pricing_price_overlay_precedence",
    "uq_pricing_price_scope_key_current",
    "uq_pricing_price_scope_key_draft",
];

/// Every index the chain declares, **by name** — the whole set, of which
/// [`EXPECTED_PARTIAL_INDEXES`] above is the members whose predicate *is* the rule
/// (Z6-5).
///
/// Two rosters over one set is deliberate and not duplication: the partial one is
/// asserted against a `WHERE`-filtered query, so it also proves those members are
/// still *partial* — dropping a predicate leaves the name in this list and removes
/// it from that one. A single roster could not tell those apart. No count is
/// written here for `EXPECTED_PARTIAL_INDEXES`'s length: it said "twelve" the day
/// `excl_pricing_price_window_no_overlap`'s `EXCLUDE` made it thirteen, which is the same rot the
/// module doc records at the top of this file. The two lists are also not quite
/// the same set now — a constraint-backed index is excluded from this one by
/// [`INDEXES_SQL`] and selected by the partial query, which is why
/// `excl_pricing_price_window_no_overlap` is in that roster and not in this one.
///
/// This list is `tests/sqlite_migrations.rs`'s `EXPECTED_INDEXES` verbatim, which
/// is the convention `EXPECTED_CHECKS` and `EXPECTED_TRIGGERS` already follow:
/// one roster per engine, so a missing statement in **one** arm of a migration
/// reddens on that engine rather than being averaged away by a shared list.
const EXPECTED_INDEXES: &[&str] = &[
    "idx_pricing_approval_key_approval",
    "idx_pricing_approval_subject",
    "idx_pricing_audit_log_recorded",
    "idx_pricing_audit_log_subject",
    "idx_pricing_bulk_operation_live",
    "idx_pricing_bulk_row_lock_operation",
    "idx_pricing_bundle_component_plan",
    "idx_pricing_bundle_component_revision",
    "idx_pricing_bundle_revshare_group_revision",
    "idx_pricing_bundle_revshare_revision",
    "idx_pricing_bundle_tenant",
    "idx_pricing_catalog_version_ref_version",
    "idx_pricing_composite_meter_revision",
    "idx_pricing_group_membership_payer",
    "idx_pricing_group_membership_walk",
    "idx_pricing_idempotency_dedup_created",
    "idx_pricing_migration_due",
    "idx_pricing_migration_source",
    "idx_pricing_migration_target",
    "idx_pricing_operator_flag_by_flag",
    "idx_pricing_outbox_undrained",
    "idx_pricing_plan_addon_rule_revision",
    "idx_pricing_plan_descriptor_set_revision",
    "idx_pricing_plan_period_floor_cap_revision",
    "idx_pricing_plan_phase_revision",
    "idx_pricing_plan_tenant",
    "idx_pricing_price_overlay_line_amount_tenant",
    "idx_pricing_price_overlay_line_plan",
    "idx_pricing_price_overlay_line_revision",
    "idx_pricing_price_overlay_scope",
    "idx_pricing_price_plan",
    "idx_pricing_price_supersedes",
    "idx_pricing_price_tier_band_price",
    "idx_pricing_price_window_due",
    "idx_pricing_price_window_price",
    "idx_pricing_read_model_resolve",
    "idx_pricing_snapshot_provenance_plan",
    "uq_pricing_approval_key_pending",
    "uq_pricing_approval_policy_pending",
    // D-307's physical half, and the one this roster exists for: it is not partial,
    // so a roster filtering on a predicate misses it entirely.
    // `the_client_key_index_spans_the_kind_as_well_as_the_tenant`
    // below asserts its columns, because a name cannot carry them.
    "uq_pricing_bulk_operation_client_key",
    "uq_pricing_bundle_plan",
    "uq_pricing_composite_meter_output",
    "uq_pricing_outbox_dedup_key",
    "uq_pricing_outbox_sequence",
    "uq_pricing_plan_current",
    "uq_pricing_plan_open_draft",
    "uq_pricing_plan_phase_terminal",
    "uq_pricing_price_meter_line_current",
    "uq_pricing_price_overlay_line_key",
    "uq_pricing_price_overlay_open_draft",
    "uq_pricing_price_overlay_precedence",
    "uq_pricing_price_scope_key_current",
    "uq_pricing_price_scope_key_draft",
    "uq_pricing_snapshot_provenance_subscription",
];

/// Every CHECK constraint the chain declares, **by name**.
///
/// Pinned as a roster rather than as a **count**, because a count cannot tell a guard
/// from a tautology: dropping `chk_pricing_plan_revision` and adding `CHECK (1 = 1)` in
/// its place leaves the count exactly where it was while a plan revision of `-999`
/// reaches the table. Every constraint here is uniquely named, so the roster costs
/// nothing the count saved.
///
/// **The number is deleted rather than corrected.** This paragraph said `== 62` twice
/// while the roster below held eighty entries — the file's own opening denounces
/// exactly that shape and had already deleted one number ten lines above. A count
/// beside a roster is one fact with two spellings and only the roster stays true.
/// Seeded from the live server once and hand-checked against the declaring
/// migrations (D-236) — see the `SQLite` roster's note for which four were read
/// back that way and why a roster taken from the code's own output pins the bug.
const EXPECTED_PRIMARY_KEYS: &[&str] = &[
    "coord_leases: key",
    "pricing_approval: approval_id",
    "pricing_approval_key: approval_id, scope_key",
    "pricing_approval_threshold: tenant_id, version, currency",
    "pricing_approval_threshold_tombstone: tenant_id, version",
    "pricing_audit_log: tenant_id, chain_id, seq",
    "pricing_brand_taxonomy: tenant_id, value",
    "pricing_bulk_operation: operation_id",
    "pricing_bulk_row_lock: tenant_id, price_id",
    "pricing_bundle: bundle_id",
    "pricing_bundle_component: bundle_id, plan_revision, component_plan_id",
    "pricing_bundle_revshare: bundle_id, plan_revision, vendor_sku_id, party",
    "pricing_bundle_revshare_group: bundle_id, plan_revision, vendor_sku_id",
    "pricing_catalog_version_ref: tenant_id, pending_ref, subject_kind, subject_ref",
    // Tenant- and plan-scoped (D-340). `composite_id, plan_revision` alone is a
    // client-supplied id with no tenant, so one composite id would belong to one
    // plan per revision *number* across the whole table. `pricing_plan_phase`
    // carries the same shape for the same reason, and each table's migration doc
    // names the other as its twin.
    "pricing_composite_meter: tenant_id, plan_id, plan_revision, composite_id",
    "pricing_customer_group_taxonomy: tenant_id, value",
    // D-356 (`pricing_gl_code_taxonomy`): the taxonomies' key on its own table.
    "pricing_gl_code_taxonomy: tenant_id, value",
    // Slice 9's membership plane (`inst-cg-record`). Keyed on its own surrogate
    // id; D-09's non-overlap is `excl_pricing_group_membership_no_overlap`'s
    // job, not the primary key's.
    "pricing_group_membership: membership_id",
    "pricing_idempotency_dedup: tenant_id, operation, client_key",
    // Client-supplied (`inst-ms-api`, M2), and therefore **tenant-scoped since
    // `pricing_migration`**: it was `migration_id` alone until 2026-08-11, which put
    // a client-chosen identifier in a deployment-wide namespace and let one tenant
    // deny an id to every other permanently. The order matters as much as the
    // membership here — `(tenant_id, migration_id)` is also the index every
    // tenant-scoped read of this table uses, which is why
    // `idx_pricing_migration_tenant` was dropped rather than kept beside it.
    "pricing_migration: tenant_id, migration_id",
    "pricing_operator_flag: tenant_id, subject_ref, flag",
    "pricing_org_tier_taxonomy: tenant_id, value",
    "pricing_outbox: outbox_id",
    "pricing_partner_taxonomy: tenant_id, value",
    "pricing_pin_frontier: tenant_id",
    "pricing_plan: plan_id, revision",
    "pricing_plan_addon_rule: plan_id, plan_revision, addon_sku_id",
    "pricing_plan_descriptor_set: plan_id, plan_revision",
    "pricing_plan_period_floor_cap: plan_id, plan_revision, currency, region",
    // **Widened by `pricing_plan_phase` (D-340)**: it was `phase_id, plan_revision`
    // until 2026-08-17, which gave one phase id to one plan per revision *number*
    // across the whole table, every tenant's included — five stand drafts keyed
    // price rows on one id and four of them could never attach it, unrecoverably.
    // The order is the tuple `idx_pricing_plan_phase_revision` already ranged over,
    // and `PRIMARY_KEYS_SQL` reads `conkey`'s own order, so a key rearranged into a
    // different key reads differently here.
    "pricing_plan_phase: tenant_id, plan_id, plan_revision, phase_id",
    "pricing_policy_object: tenant_id",
    "pricing_price: price_id",
    "pricing_price_overlay: price_overlay_id, revision",
    // Both widened by `pricing_price_overlay_line_amount` (A1-3, and A1-4 for the child),
    // 2026-08-18: a client-supplied `line_id` with no tenant in the key, and a
    // child whose key had to move with the parent's or collide on the amounts
    // instead.
    "pricing_price_overlay_line: tenant_id, overlay_revision, line_id",
    "pricing_price_overlay_line_amount: tenant_id, overlay_revision, line_id, currency",
    "pricing_price_tier_band: band_id",
    "pricing_price_window: window_id",
    "pricing_read_model: tenant_id, catalog_version, subject_kind, subject_ref",
    "pricing_region_taxonomy: tenant_id, value",
    "pricing_repricing_journal: run_id, price_id",
    // D-334 (`pricing_rounding_policy_taxonomy`): the taxonomies' key on its own table.
    "pricing_rounding_policy_taxonomy: tenant_id, value",
    // Read back from `pricing_snapshot_provenance`'s own DDL rather than from the live server.
    "pricing_snapshot_provenance: provenance_id",
];

const EXPECTED_CHECKS: &[&str] = &[
    "chk_pricing_approval_approver",
    "chk_pricing_approval_decided_at",
    "chk_pricing_approval_distinct_principals",
    "chk_pricing_approval_key_state",
    "chk_pricing_approval_reason",
    "chk_pricing_approval_state",
    "chk_pricing_approval_subject_kind",
    "chk_pricing_approval_threshold_absolute_non_negative",
    "chk_pricing_approval_threshold_basis",
    "chk_pricing_approval_threshold_currency",
    "chk_pricing_approval_threshold_percent_positive",
    "chk_pricing_approval_threshold_tombstone_version",
    "chk_pricing_approval_threshold_version",
    "chk_pricing_audit_log_action",
    "chk_pricing_audit_log_entry_kind",
    "chk_pricing_audit_log_rollup",
    "chk_pricing_audit_log_seq",
    // Z6-6 (`pricing_audit_log`), the same name the SQLite mirror carries: one enum
    // spells two columns and only `pricing_approval`'s was CHECK-constrained.
    "chk_pricing_audit_log_subject_kind",
    "chk_pricing_brand_taxonomy_state",
    "chk_pricing_brand_taxonomy_value_present",
    // Slice 12's bulk operation, the same four the SQLite mirror carries.
    "chk_pricing_bulk_operation_completed_at",
    "chk_pricing_bulk_operation_import_never_awaits",
    "chk_pricing_bulk_operation_kind",
    "chk_pricing_bulk_operation_state",
    "chk_pricing_bundle_component_min_qty",
    "chk_pricing_bundle_component_qty_range",
    "chk_pricing_bundle_invoice_itemization",
    "chk_pricing_bundle_price_basis",
    "chk_pricing_bundle_revshare_effective_share_bp",
    "chk_pricing_bundle_revshare_group_absorber",
    "chk_pricing_bundle_revshare_group_platform_cut_bp",
    "chk_pricing_bundle_revshare_party",
    "chk_pricing_bundle_revshare_share_bp",
    "chk_pricing_catalog_version_ref_commit",
    "chk_pricing_catalog_version_ref_subject_kind",
    "chk_pricing_catalog_version_ref_subject_lifecycle",
    "chk_pricing_catalog_version_ref_subject_revision",
    "chk_pricing_catalog_version_ref_version",
    // Slice 10's composite meter. One CHECK only: arity and self-reference are
    // publish rules, for `pricing_composite_meter`'s portability reason.
    "chk_pricing_composite_meter_output_unit",
    // Slice 9's own taxonomy (`inst-cg-taxonomy`), the four's own two CHECKs
    // restated over `pricing_customer_group_taxonomy` — see that table's
    // migration doc for why it is on its own route and not filed under
    // `config`'s four.
    "chk_pricing_customer_group_taxonomy_state",
    "chk_pricing_customer_group_taxonomy_value_present",
    // D-356's declared GL-code vocabulary, the same two the SQLite mirror carries.
    "chk_pricing_gl_code_taxonomy_state",
    "chk_pricing_gl_code_taxonomy_value_present",
    // Slice 9's membership plane (`inst-cg-record`): the value-present guard the
    // four taxonomies also carry, the half-open interval sanity check
    // `pricing_price_window`/`pricing_price_overlay` carry too, and the entity
    // tag's floor. D-09's
    // non-overlap invariant is `excl_pricing_group_membership_no_overlap`, a
    // separate `contype = 'x'` object `CHECKS_SQL` does not select (`contype =
    // 'c'` only) and does not belong in this roster.
    "chk_pricing_group_membership_group_value_present",
    "chk_pricing_group_membership_interval",
    "chk_pricing_group_membership_row_version",
    "chk_pricing_idempotency_dedup_answered",
    "chk_pricing_idempotency_dedup_status",
    // Slice 11, the same twelve the SQLite mirror carries, name for name.
    "chk_pricing_migration_announced_before_effective",
    "chk_pricing_migration_cancelled_at",
    "chk_pricing_migration_cancelled_order",
    "chk_pricing_migration_completed_at",
    "chk_pricing_migration_completed_order",
    "chk_pricing_migration_distinct_plans",
    "chk_pricing_migration_exclusion_snapshot",
    "chk_pricing_migration_scheduled_unstarted",
    "chk_pricing_migration_source_revision",
    "chk_pricing_migration_started_order",
    "chk_pricing_migration_started_required",
    "chk_pricing_migration_state",
    "chk_pricing_operator_flag_name",
    "chk_pricing_org_tier_taxonomy_state",
    "chk_pricing_org_tier_taxonomy_value_present",
    "chk_pricing_outbox_event_name",
    "chk_pricing_outbox_sequence",
    "chk_pricing_partner_taxonomy_state",
    "chk_pricing_partner_taxonomy_value_present",
    "chk_pricing_pin_frontier_version",
    "chk_pricing_plan_addon_rule_max_qty",
    "chk_pricing_plan_addon_rule_min_qty",
    "chk_pricing_plan_addon_rule_qty_range",
    "chk_pricing_plan_addon_rule_required_max_qty",
    "chk_pricing_plan_addon_rule_step_qty",
    "chk_pricing_plan_availability",
    "chk_pricing_plan_billing_cycle",
    "chk_pricing_plan_custom_interval_n",
    "chk_pricing_plan_custom_interval_pairing",
    "chk_pricing_plan_custom_interval_unit",
    "chk_pricing_plan_frequency",
    "chk_pricing_plan_lifecycle_state",
    "chk_pricing_plan_period_floor_cap_cap_positive",
    "chk_pricing_plan_period_floor_cap_currency",
    "chk_pricing_plan_period_floor_cap_floor_positive",
    "chk_pricing_plan_period_floor_cap_ordered",
    "chk_pricing_plan_period_floor_cap_present",
    "chk_pricing_plan_phase_display_trial_days",
    "chk_pricing_plan_phase_duration_non_negative",
    "chk_pricing_plan_phase_kind",
    "chk_pricing_plan_phase_trial_projection_non_negative",
    "chk_pricing_plan_purchase_max_qty",
    "chk_pricing_plan_purchase_min_qty",
    "chk_pricing_plan_purchase_qty",
    "chk_pricing_plan_revision",
    "chk_pricing_plan_row_version",
    "chk_pricing_policy_object_interval_days_cap",
    "chk_pricing_policy_object_interval_months_cap",
    "chk_pricing_policy_object_notice_floor",
    "chk_pricing_policy_object_price_row_cap",
    // Slice 4's C4 switch, and since D-240 the only tax-display constraint on
    // this table.
    // `chk_pricing_policy_object_tax_display` would sit above it, holding
    // a display *basis* default under a name section 6 spends on this
    // fail-closed *enforcement* mode; retiring it is what makes the name
    // unambiguous rather than merely adjacent.
    "chk_pricing_policy_object_tax_display_policy",
    // `chk_pricing_policy_object_threshold` and its `_non_negative` sibling are
    // deliberately absent: `pricing_approval_threshold` is where the threshold lives, and the two columns they guarded are gone from `pricing_policy_object`
    // when the threshold moved to `pricing_approval_threshold`, and a CHECK over a
    // column that no longer exists is what a stale claim looks like.
    "chk_pricing_policy_object_tier_band_cap",
    "chk_pricing_price_aggregation_function",
    "chk_pricing_price_aggregation_granularity",
    "chk_pricing_price_amount_non_negative",
    "chk_pricing_price_billing_granularity",
    "chk_pricing_price_billing_timing",
    "chk_pricing_price_charge_kind",
    "chk_pricing_price_cohort_eligibility",
    "chk_pricing_price_eligibility",
    "chk_pricing_price_grandfather_until",
    "chk_pricing_price_lifecycle_state",
    "chk_pricing_price_manual_quantity",
    "chk_pricing_price_max_hold_granules",
    "chk_pricing_price_meter_no_separator",
    "chk_pricing_price_min_qty_purchase",
    "chk_pricing_price_min_qty_usage",
    "chk_pricing_price_model_kind",
    "chk_pricing_price_overlay",
    // Slice 9's overlay object. `chk_pricing_price_overlay` one line up is the
    // **price row's** `price_overlay` axis CHECK (always `base`); everything from
    // here down belongs to the overlay object, which is a separate row.
    "chk_pricing_price_overlay_disclosure",
    "chk_pricing_price_overlay_interval",
    "chk_pricing_price_overlay_lifecycle_state",
    "chk_pricing_price_overlay_line_adjustment_kind",
    "chk_pricing_price_overlay_line_amount_currency",
    "chk_pricing_price_overlay_line_amount_value_minor",
    "chk_pricing_price_overlay_line_cohort_needs_plan",
    "chk_pricing_price_overlay_line_discount_ceiling",
    "chk_pricing_price_overlay_line_fixed_is_amount",
    "chk_pricing_price_overlay_line_magnitude_kind",
    "chk_pricing_price_overlay_line_magnitude_pairing",
    "chk_pricing_price_overlay_line_magnitude_positive",
    "chk_pricing_price_overlay_line_plan_id_not_nil",
    "chk_pricing_price_overlay_line_sku_needs_plan",
    "chk_pricing_price_overlay_line_target_sku_present",
    "chk_pricing_price_overlay_revision",
    "chk_pricing_price_overlay_row_version",
    "chk_pricing_price_overlay_scope_class",
    "chk_pricing_price_overlay_scope_value",
    "chk_pricing_price_overlay_tax_basis",
    "chk_pricing_price_package_fields_kind",
    "chk_pricing_price_package_price",
    "chk_pricing_price_package_size",
    "chk_pricing_price_quantity_source",
    "chk_pricing_price_region_no_separator",
    "chk_pricing_price_reserved_rate_nano",
    "chk_pricing_price_row_version",
    "chk_pricing_price_tier_aggregation_window",
    "chk_pricing_price_tier_band_from_qty",
    "chk_pricing_price_tier_band_unit_price",
    "chk_pricing_price_tier_band_width",
    "chk_pricing_price_tier_qualification_window",
    // D-311's `per_unit` rate, non-negative for the reason `amount_minor` is:
    // typed credit rows are Future scope, so a negative price is a mistake
    // caught where it lands. Postgres only -- `pricing_price`'s migration doc
    // records why `SQLite` carries no twin and what holds the rule there instead.
    "chk_pricing_price_unit_rate_nano",
    "chk_pricing_price_window_activated_at",
    "chk_pricing_price_window_activation_order",
    "chk_pricing_price_window_cancelled_at",
    "chk_pricing_price_window_expired_at",
    "chk_pricing_price_window_expiry_order",
    "chk_pricing_price_window_interval",
    "chk_pricing_price_window_mutation_seq",
    "chk_pricing_price_window_open_ended",
    "chk_pricing_price_window_reason_code",
    "chk_pricing_price_window_state",
    "chk_pricing_read_model_catalog_version",
    "chk_pricing_read_model_subject_kind",
    "chk_pricing_read_model_warm_marker",
    "chk_pricing_region_taxonomy_state",
    "chk_pricing_region_taxonomy_value_present",
    // Slice 12's repricing journal, the same four the SQLite mirror carries.
    "chk_pricing_repricing_journal_applied",
    "chk_pricing_repricing_journal_failed",
    "chk_pricing_repricing_journal_state",
    "chk_pricing_repricing_journal_successor_is_new",
    "chk_pricing_rounding_policy_taxonomy_state",
    "chk_pricing_rounding_policy_taxonomy_value_present",
    "chk_pricing_snapshot_provenance_payload",
    "chk_pricing_snapshot_provenance_resolved",
    "chk_pricing_snapshot_provenance_revision",
    "chk_pricing_snapshot_provenance_trigger",
];

// ---------------------------------------------------------------------------
// The chain through production's runner
// ---------------------------------------------------------------------------

/// Boot 1 applies everything and boot 2 applies nothing.
///
/// This is the C1 regression the sibling ledger pinned, asked of this gear. The
/// hazard is the runner's *unqualified* bookkeeping table: with `bss` first in
/// the path it lands in `public` on boot 1 (before `bss` exists) and a **second,
/// empty** one is created in `bss` on boot 2, whereupon the history reads empty,
/// every migration re-runs, and a non-`IF NOT EXISTS` `CREATE TABLE` aborts the
/// boot. `public` first is the arrangement that cannot do that.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_second_boot_applies_nothing_under_a_public_first_search_path() {
    let (port, _guard) = pg().await;
    let db = connect_db(
        &url_with_search_path(port, "public,bss"),
        ConnectOpts::default(),
    )
    .await
    .expect("connect with a public,bss search_path");

    let first = run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("boot 1 must apply the whole chain");
    assert_eq!(first.applied, chain_len(), "boot 1 applies every migration");

    let second = run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("boot 2 must be a clean no-op");
    assert_eq!(second.applied, 0, "boot 2 applies nothing");
    assert_eq!(second.skipped, chain_len(), "boot 2 skips every migration");
}

/// **This gear carries C1, and the second boot crash-loops under `bss,public`.**
///
/// Found 2026-08-03 by the first run of this suite, and it is a defect rather
/// than a test artifact: with `bss` first, boot 1 puts the runner's *unqualified*
/// bookkeeping table in `public` because `bss` does not exist yet; boot 2 finds
/// `bss` first, creates a **second, empty** bookkeeping table there, reads an
/// empty history, and re-runs the chain into
/// `relation "coord_leases" already exists`.
///
/// The chain's module doc argues this gear is safe here, and it argues the wrong
/// half: `pricing_plan` and coord's `m0001_…` do both issue `CREATE SCHEMA IF
/// NOT EXISTS bss`, so *schema creation* is order-proof — but C1 is about where
/// the **bookkeeping** table resolves, which no `IF NOT EXISTS` affects.
///
/// Nothing in this repository configures a `search_path` for this gear, so the
/// hazard is latent rather than live: the server default puts bookkeeping in
/// `public` and the boot above is the one that happens. It becomes live the day a
/// deployment sets `bss,public`, which is the arrangement the sibling ledger
/// shipped and had to fix.
///
/// Pinned as executable documentation in ledger's own idiom
/// (`postgres_migration_idempotency.rs::bss_first_search_path_crash_loops_on_second_boot`).
/// **When the hazard is closed this test reddens, and that is deliberate** — it
/// forces whoever closes it to invert the assertion rather than to discover
/// later that the fix was never exercised.
#[tokio::test]
#[ignore = "requires Docker (testcontainers); documents the C1 crash"]
async fn a_bss_first_search_path_crash_loops_on_the_second_boot() {
    let (port, _guard) = pg().await;
    let db = connect_db(
        &url_with_search_path(port, "bss,public"),
        ConnectOpts::default(),
    )
    .await
    .expect("connect with a bss,public search_path");

    let first = run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("boot 1 succeeds even under the hazardous order");
    assert_eq!(first.applied, chain_len());

    let second = run_migrations_for_testing(&db, Migrator::migrations()).await;
    assert!(
        second.is_err(),
        "boot 2 under bss,public must reproduce C1; got {second:?}"
    );

    let raw = Database::connect(&plain_url(port))
        .await
        .expect("connect plainly");
    let bookkeeping = count(
        &raw,
        "SELECT count(*)::bigint AS n FROM information_schema.tables \
         WHERE table_name LIKE 'toolkit_migrations%'",
    )
    .await;
    assert_eq!(
        bookkeeping, 2,
        "and the mechanism is the duplicate bookkeeping table, not something else"
    );
}

/// Run one statement that must land, for the staged run's world-building.
async fn must_succeed(conn: &DatabaseConnection, sql: &str) {
    conn.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .unwrap_or_else(|e| panic!("statement must succeed: {sql}\n{e}"));
}

// ---------------------------------------------------------------------------
// What reached the server
// ---------------------------------------------------------------------------

/// A chain applied through the runner, for the census tests to inspect.
async fn applied() -> (DatabaseConnection, ContainerAsync<Postgres>) {
    let (port, guard) = pg().await;
    let db = connect_db(
        &url_with_search_path(port, "public,bss"),
        ConnectOpts::default(),
    )
    .await
    .expect("connect");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("apply the chain");
    let raw = Database::connect(&plain_url(port))
        .await
        .expect("connect plainly");
    (raw, guard)
}

/// The PL/pgSQL functions and their triggers, by name.
///
/// What this does **not** say: that any body is correct. `check_function_bodies`
/// is a syntax check — a trigger function may reference a column that does not
/// exist and still be created, failing only when it fires. Whether these
/// triggers refuse what they claim to is Track P's, and needs DML this suite
/// deliberately does not issue.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_declared_trigger_function_and_trigger_reaches_the_server() {
    let (conn, _guard) = applied().await;
    assert_eq!(names(&conn, FUNCTIONS_SQL).await, EXPECTED_FUNCTIONS);
    assert_eq!(names(&conn, TRIGGERS_SQL).await, EXPECTED_TRIGGERS);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_table_key_reaches_the_server_with_the_columns_and_the_order_it_was_declared_in() {
    // The Postgres half of D-236's roster. The `SQLite` half is on the fast tier on
    // purpose: D-236's own finding is that the pin which caught `pricing_catalog_version_ref`
    // was `#[ignore]`d behind Docker, so a run without it reported a clean change —
    // "one premise duplicated across tiers breaks in instalments", arriving from the
    // direction where the premise lived on *one* tier only. This half exists because
    // the two engines declare these keys in separate statements and could drift.
    let (conn, _guard) = applied().await;
    assert_eq!(
        names(&conn, PRIMARY_KEYS_SQL).await,
        EXPECTED_PRIMARY_KEYS,
        "a primary key that lost a column, gained one, or reordered: the one piece of DDL whose \
         loss first shows up as a duplicate row in a table whose whole contract is that it has none"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_declared_check_constraint_reaches_the_server_by_name() {
    let (conn, _guard) = applied().await;
    assert_eq!(
        names(&conn, CHECKS_SQL).await,
        EXPECTED_CHECKS,
        "a CHECK that vanished or was renamed is a guard nobody removed on purpose"
    );
}

/// **Every foreign key, table `UNIQUE` and `EXCLUDE` reaches the server, with the
/// columns and the parent it was declared with.**
///
/// The roster the census was missing: `contype` `'f'`, `'u'` and `'x'` were
/// selected by no query here, and — because a `BEFORE` trigger answers ahead of
/// constraint checking on every one of these child tables — could not be proved
/// by refusal in the schema suites either. A key dropped from a Postgres arm was
/// invisible on both tiers.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_relational_constraint_reaches_the_server_with_its_definition() {
    let (conn, _guard) = applied().await;
    assert_eq!(
        names(&conn, RELATIONAL_CONSTRAINTS_SQL).await,
        EXPECTED_RELATIONAL_CONSTRAINTS,
        "a foreign key, UNIQUE or EXCLUDE that vanished, was renamed, or was rebuilt over \
         different columns: the class of DDL no refusal in this gear can reach, because a \
         BEFORE trigger answers first on every table that carries one"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_declared_partial_index_reaches_the_server() {
    let (conn, _guard) = applied().await;
    assert_eq!(
        names(&conn, PARTIAL_INDEXES_SQL).await,
        EXPECTED_PARTIAL_INDEXES
    );
}

/// **Every index the chain declares reaches the server, by name** — Z6-5.
///
/// The suite's only index assertion used to be the partial one above, and its SQL
/// filters on `indexdef LIKE '%WHERE%'`. So the roster covered **12 of 51**, and
/// the 39 without a predicate had no Postgres assertion at all — including
/// `uq_pricing_bulk_operation_client_key`, the physical half of D-307's cross-kind
/// admission. An index missing from a Postgres arm, or restated with the wrong
/// columns, is invisible on the engine that ships.
///
/// Constraint-backed indexes are excluded rather than rostered: a primary key's
/// index is created by the constraint, is named by Postgres rather than by this
/// chain, and has its own roster in `PRIMARY_KEYS_SQL`. So what this ranges over is
/// exactly the indexes the migrations write `CREATE INDEX` for — which is what the
/// `SQLite` roster ranges over too, and why the two lists are the same names.
///
/// The count is deliberately not written here. A number in prose beside the list it
/// counts goes stale on the next index — `pricing_snapshot_provenance`'s reached the
/// server after the literal was written — and the list is the measurement, so a
/// literal beside it can only ever go stale or be right by accident.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_declared_index_reaches_the_server_by_name() {
    let (conn, _guard) = applied().await;
    assert_eq!(
        names(&conn, INDEXES_SQL).await,
        EXPECTED_INDEXES,
        "an index that vanished, was renamed, or was never written into the Postgres arm of its \
         migration is a read path nobody removed on purpose - and on the engine that ships, \
         nothing else in this suite would have noticed"
    );
}

/// **The D-307 index carries the kind, and it is measured on both engines.**
///
/// A name roster cannot see an index restated over the wrong columns, and this is
/// the index it happens to: a `pricing_bulk_operation` rebuild that writes
/// `uq_pricing_bulk_operation_client_key` with the pre-D-307 columns passes every
/// name census on both engines.
///
/// `indexdef` and not a count of columns: what D-307 decided is *which* axes the
/// key spans, so the assertion names them. `pricing_price`'s migration doc states the
/// principle this applies — "a measurement on one engine is not a fact about the
/// other".
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_client_key_index_spans_the_kind_as_well_as_the_tenant() {
    let (conn, _guard) = applied().await;
    let definitions = names(
        &conn,
        "SELECT indexdef AS v FROM pg_indexes WHERE schemaname = 'bss' \
         AND indexname = 'uq_pricing_bulk_operation_client_key'",
    )
    .await;
    let definition = definitions
        .first()
        .expect("uq_pricing_bulk_operation_client_key must exist on the server");
    assert!(
        definition.contains("(tenant_id, kind, client_key)"),
        "D-307 keys a client key per kind, so one run id opens one import and one repricing run \
         alike; this index reads {definition}"
    );
}

/// **The bundle plan slot is per tenant** — `pricing_bundle`, A1-2.
///
/// Its name is in `EXPECTED_INDEXES` and was there before the widening, which is
/// exactly why this case exists: a name cannot carry columns, and the widening
/// deliberately kept the name so the constraint keeps one spelling across the
/// change. Nothing in the name census could tell `(plan_id)` from
/// `(tenant_id, plan_id)`.
///
/// The narrow form was the only opinion the schema had about a `plan_id` a client
/// puts in a request body — `pricing_bundle` carries no foreign key at all — so
/// the first tenant to name one locked every other tenant out of it permanently,
/// against a row invisible to them and with no `DELETE` in the API.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_bundle_plan_slot_is_unique_per_tenant_rather_than_globally() {
    let (conn, _guard) = applied().await;
    let definitions = names(
        &conn,
        "SELECT indexdef AS v FROM pg_indexes WHERE schemaname = 'bss' \
         AND indexname = 'uq_pricing_bundle_plan'",
    )
    .await;
    let definition = definitions
        .first()
        .expect("uq_pricing_bundle_plan must exist on the server");
    assert!(
        definition.contains("(tenant_id, plan_id)"),
        "a plan's bundle slot belongs to the tenant that owns the plan; this index reads \
         {definition}"
    );
}

/// **Every revision column in the chain is `bigint`** — Z6-7.
///
/// A plan revision is a `u64` wherever it is a value, and `pricing_plan.revision`
/// is `bigint`. Two columns were `integer` — `pricing_migration.source_revision`
/// and `pricing_snapshot_provenance.source_revision` — which made them addressable
/// to 2^31-1, guarded at the boundary by an `i32::try_from` that answered
/// `CorruptRow`. Both are `bigint` on `pricing_snapshot_provenance` and `pricing_migration`.
///
/// The property is stated over the **schema** rather than over those two columns,
/// which is the point: a spot check on the two known outliers would be green
/// against the third. It reads `information_schema` after the whole chain, so a
/// later `CREATE TABLE` that types a revision `integer` reddens here — including
/// through a rebuild that restates a column verbatim, which is exactly how
/// `pricing_migration` carried this outlier forward without anyone seeing it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_revision_column_is_bigint() {
    let (conn, _guard) = applied().await;
    let columns = names(&conn, REVISION_COLUMNS_SQL).await;
    assert_eq!(
        columns, EXPECTED_REVISION_COLUMNS,
        "the roster and the schema must agree; a column added here owes a line in \
         `EXPECTED_REVISION_COLUMNS`, and one that vanished from the scan is a \
         broken query or a dropped column"
    );
    let narrow: Vec<&String> = columns
        .iter()
        .filter(|column| !column.ends_with(" bigint"))
        .collect();
    assert!(
        narrow.is_empty(),
        "a revision column narrower than the u64 a revision is: {narrow:?}"
    );
}

// ---------------------------------------------------------------------------
// Down, and back up
// ---------------------------------------------------------------------------

/// A rolled-back chain leaves no table **and no function**.
///
/// Functions are the class this can actually miss: indexes, triggers and CHECKs
/// go with their table by cascade, while every PL/pgSQL function needs an
/// explicit `DROP FUNCTION` in its migration's `down`. Deleting **any one** of those
/// statements leaves an orphan a tables-only assertion cannot see.
///
/// The number is deleted rather than corrected, for `EXPECTED_CHECKS`' reason: it read
/// "one of those nine statements" over fifteen, and what the test ranges over is the
/// chain's functions as the chain declares them rather than a count of `down` bodies.
///
/// The `bss` schema itself is expected to **survive**: coord and the sibling
/// gears live there, and a `down` that dropped it would take their tables with
/// it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_chain_rolls_back_leaving_no_table_and_no_function() {
    let (port, _guard) = pg().await;
    let conn = Database::connect(&plain_url(port))
        .await
        .expect("connect postgres");
    Migrator::up(&conn, None).await.expect("apply the chain");
    Migrator::down(&conn, None)
        .await
        .expect("the whole chain must roll back");

    let tables = count(
        &conn,
        "SELECT count(*)::bigint AS n FROM pg_tables WHERE schemaname = 'bss'",
    )
    .await;
    assert_eq!(tables, 0, "a rolled-back chain leaves no table in `bss`");

    let functions = count(
        &conn,
        "SELECT count(*)::bigint AS n FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'bss'",
    )
    .await;
    assert_eq!(
        functions, 0,
        "nor an orphan PL/pgSQL function: each `down` must drop its own"
    );

    let schema = count(
        &conn,
        "SELECT count(*)::bigint AS n FROM information_schema.schemata \
         WHERE schema_name = 'bss'",
    )
    .await;
    assert_eq!(
        schema, 1,
        "and the shared schema survives, because coord and the sibling gears live in it"
    );
}

/// The re-entry that is actually evidence: down, then up again.
///
/// Applying twice in a row proves nothing — the runner filters what it has
/// already booked, so the second call executes no statements at all and the
/// chain's own SQL is not idempotent (`CREATE TABLE` without `IF NOT EXISTS`).
/// Rolling back and re-applying is the run that reaches the statements, and it
/// answers this program's standing question: what does the **second** run of the
/// mechanism read?
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_chain_survives_a_roll_back_and_a_re_apply() {
    let (port, _guard) = pg().await;
    let conn = Database::connect(&plain_url(port))
        .await
        .expect("connect postgres");

    Migrator::up(&conn, None).await.expect("apply");
    let before_functions = names(&conn, FUNCTIONS_SQL).await;
    let before_checks = names(&conn, CHECKS_SQL).await;
    let before_indexes = names(&conn, PARTIAL_INDEXES_SQL).await;

    Migrator::down(&conn, None).await.expect("roll back");
    Migrator::up(&conn, None)
        .await
        .expect("the chain must re-apply onto the ground it cleared");

    assert_eq!(names(&conn, FUNCTIONS_SQL).await, before_functions);
    assert_eq!(names(&conn, CHECKS_SQL).await, before_checks);
    assert_eq!(names(&conn, PARTIAL_INDEXES_SQL).await, before_indexes);
}

/// **The publish commit's approval read is served by an index and not by a scan.**
///
/// Every commit path in the crate runs this predicate — `infra::retirement` runs
/// it *inside* the retirement transaction, and `infra::cutover`,
/// `infra::supersession`, `infra::window` and `infra::grandfather` each run it on
/// their own. `pricing_approval` is `DELETE`-refused and has no purge job, so
/// without an index the cost grows with the retention horizon and with every other
/// tenant's history rather than with the plan being published.
///
/// **Armed against the plan shape, and seeded on purpose.** Two ways this probe
/// could be green while proving nothing, both avoided here:
///
/// * On the empty database every other test in this file leaves behind, Postgres
///   picks a sequential scan whatever indexes exist, because a scan of nothing is
///   the cheapest plan there is. So the table is seeded and `ANALYZE`d first — the
///   assertion is about which plan the planner *chooses* when it has a choice.
/// * "An index is used" is satisfied by the primary key's index doing a full pass.
///   So the index is named, and the absence of `Seq Scan` is asserted separately.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_approval_subject_read_is_served_by_an_index() {
    /// The one subject the plan is measured for. `100 % 100 = 0`, so it is a row
    /// the queried tenant owns — see the seed below.
    const SUBJECT_UNDER_TEST: &str = "100/1";

    let (conn, _guard) = applied().await;
    let tenant = "11111111-1111-1111-1111-111111111111";

    // One tenant's history among many, which is the shape that makes the missing
    // index expensive: the predicate selects a handful of rows out of a table that
    // grows with every other tenant.
    //
    // **Every hundredth row is the queried tenant's.** The seed used to give every
    // row a `gen_random_uuid()` tenant, so the tenant the plan is measured for
    // owned *none* of the 20 000 and the planner was asked which plan it picks for
    // a predicate selecting zero rows — not for the selective read every commit
    // path runs. `g % 100 = 0` is what makes `subject_ref = '100/1'` a row this
    // tenant actually has; the arming assertion below is what keeps it that way.
    //
    // Born submitted and decided afterwards, because that is the only way a row
    // gets into this table: `trg_pricing_approval_append_only` refuses an INSERT
    // that arrives already `approved` -- "a record is born submitted". A seed that
    // fought the guard rather than following it would be testing a state the
    // domain cannot produce.
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_approval \
             (approval_id, tenant_id, subject_ref, subject_kind, content_hash, state, \
              submitter_principal, materiality, submitted_at) \
             SELECT gen_random_uuid(), \
                    CASE WHEN g % 100 = 0 THEN '{tenant}'::uuid ELSE gen_random_uuid() END, \
                    g::text || '/1', 'plan_revision', \
                    decode(md5(g::text), 'hex'), 'submitted', gen_random_uuid(), \
                    '{{}}'::jsonb, now() \
             FROM generate_series(1, 20000) g"
        ),
    )
    .await;
    must_succeed(
        &conn,
        "UPDATE bss.pricing_approval \
         SET state = 'approved', approver_principal = gen_random_uuid(), decided_at = now() \
         WHERE state = 'submitted'",
    )
    .await;
    must_succeed(&conn, "ANALYZE bss.pricing_approval").await;

    // The probe is armed against the shape it names, not against an empty
    // predicate: the queried tenant owns rows, and the queried `subject_ref` is
    // one of them.
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*)::bigint AS n FROM bss.pricing_approval \
                 WHERE tenant_id = '{tenant}'::uuid"
            ),
        )
        .await,
        200,
        "the measured tenant must own a share of the table, or the plan below is \
         chosen for a predicate that selects nothing"
    );
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*)::bigint AS n FROM bss.pricing_approval \
                 WHERE tenant_id = '{tenant}'::uuid AND state = 'approved' \
                 AND subject_ref = '{SUBJECT_UNDER_TEST}'"
            ),
        )
        .await,
        1,
        "and the read below must find its row"
    );

    let plan = explain(
        &conn,
        &format!(
            "SELECT approval_id FROM bss.pricing_approval \
             WHERE tenant_id = '{tenant}'::uuid AND state = 'approved' \
             AND subject_ref = '{SUBJECT_UNDER_TEST}' ORDER BY decided_at ASC LIMIT 1"
        ),
    )
    .await;

    assert!(
        !plan.contains("Seq Scan"),
        "the approval read runs on every commit path and must not scan the table; plan was:\n{plan}"
    );
    assert!(
        plan.contains("idx_pricing_approval_subject"),
        "the plan must use the read-shape index and not merely some index; plan was:\n{plan}"
    );
}

/// **Every column a trigger function dereferences exists on the table it guards**
/// (review P-6).
///
/// The gap this closes, stated as itself: the Postgres census pins 30 function
/// names and 30 trigger names and **nothing about what those functions say**. Its
/// own doc admits it — *"`check_function_bodies` is a syntax check; a trigger
/// function may reference a column that does not exist and still be created,
/// failing only when it fires."* `SQLite` has no such hole, because
/// `sqlite_migrations` pins a digest for all 94 trigger bodies.
///
/// And the asymmetry is not academic: `ALTER TABLE … RENAME COLUMN` **rewrites**
/// `SQLite` trigger bodies and leaves `pg_proc.prosrc` untouched, so the one
/// defect class it produces is invisible on exactly the engine where it can
/// happen. It has happened: a `RENAME COLUMN` on `pricing_price` left
/// `bss.pricing_price_append_only()` naming the old one, taking the whole
/// append-only guarantee down until some other suite issued DML.
///
/// **Armed against dereference, not against a digest.** A digest pin over
/// `prosrc` would redden on every deliberate guard restatement — this chain has
/// six — and would be turned off within a wave. This asks the only question that
/// is always wrong to answer badly: does each `NEW.x` / `OLD.x` name a live column
/// of the table the trigger is attached to?
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_trigger_function_dereferences_only_live_columns() {
    let (conn, _guard) = applied().await;

    // One row per (function, trigger, table, dereferenced identifier), straight
    // from the catalog: `prosrc` is the body as stored, `regexp_matches` pulls
    // every `NEW.<ident>` / `OLD.<ident>` out of it, and the join to
    // `information_schema.columns` is the question.
    let orphans = names(
        &conn,
        &format!(
            "SELECT DISTINCT p.proname || ' -> ' || c.relname || '.' || m[2] AS v \
             {DEREFERENCES_OF_TRIGGER_FUNCTIONS} \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM information_schema.columns col \
                 WHERE col.table_schema = 'bss' \
                   AND col.table_name = c.relname \
                   AND col.column_name = m[2]) \
             ORDER BY 1"
        ),
    )
    .await;

    assert!(
        orphans.is_empty(),
        "a trigger function dereferences a column its table does not have; the guard is down \
         until it next fires, and nothing else in this suite can see it: {orphans:?}"
    );
}

/// The census's extraction, spelled **once**: every `NEW.<ident>` / `OLD.<ident>`
/// a trigger function dereferences, joined to the table its trigger is attached
/// to.
///
/// A `FROM` fragment rather than a whole query, because the two readers project
/// different things — the census pairs the identifier with the table so it can ask
/// `information_schema` about it, the control only needs the identifier. What they
/// must not do is carry two copies of the **pattern**: with one each, breaking the
/// census's copy left the control extracting from its own and reporting a clean
/// bill of health for a census that matched nothing.
///
/// `\\.` is a Rust-escaped `\.` reaching Postgres, where it is the regexp escape
/// for a literal dot; `n` is bound so the search stays in this gear's schema, and
/// `NOT t.tgisinternal` drops the constraint triggers the catalog also carries.
const DEREFERENCES_OF_TRIGGER_FUNCTIONS: &str = "FROM pg_proc p \
     JOIN pg_namespace n ON n.oid = p.pronamespace AND n.nspname = 'bss' \
     JOIN pg_trigger t ON t.tgfoid = p.oid AND NOT t.tgisinternal \
     JOIN pg_class c ON c.oid = t.tgrelid \
     CROSS JOIN LATERAL regexp_matches(p.prosrc, '(NEW|OLD)\\.([a-z_][a-z0-9_]*)', 'g') AS m";

/// Dereferences the extraction must find, named rather than counted.
///
/// A floor — `found.len() >= 40` and the like — is not a measurement: it passes
/// while the population collapses toward it, however much larger the population
/// really is. A named member cannot decay quietly: it is either extracted or it is
/// not.
///
/// One per guard shape the chain has, and the last six are deliberate: they are
/// the revision-scoped children whose parent-tenancy arm reads `NEW.tenant_id`, so
/// the roster also says that arm's operand is still dereferenced in each of the six
/// bodies. The other six spread the roster over the plan, price, window, migration,
/// approval and bulk planes, so a regexp that stopped matching one table's spelling
/// cannot hide behind another's.
const REQUIRED_DEREFERENCES: &[&str] = &[
    "pricing_price_append_only.lifecycle_state",
    "pricing_price_window_append_only.mutation_seq",
    "pricing_plan_append_only.tenant_id",
    "pricing_migration_append_only.state",
    "pricing_approval_append_only.state",
    "pricing_bulk_operation_transitions.state",
    "pricing_plan_phase_append_only.tenant_id",
    "pricing_plan_addon_rule_append_only.tenant_id",
    "pricing_composite_meter_append_only.tenant_id",
    "pricing_plan_descriptor_set_append_only.tenant_id",
    "pricing_plan_period_floor_cap_append_only.tenant_id",
    "pricing_price_overlay_line_append_only.tenant_id",
];

/// The anti-vacuity control for the census above.
///
/// A scan that matched nothing would report an empty orphan set and read as a
/// clean bill of health. This pins that the extraction actually finds
/// dereferences — the guards are full of them — so the emptiness above is a
/// measurement rather than a silence.
///
/// **It reads the census's own `FROM` clause**, which is the whole point of the
/// control and was the one thing it did not do: the pattern and the join were
/// written out a second time here, so breaking the census's copy of the regexp
/// left this test extracting happily from its own copy and reporting a clean bill
/// of health for a census that had gone blind. Sharing
/// [`DEREFERENCES_OF_TRIGGER_FUNCTIONS`] is what makes a broken pattern redden
/// here instead.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_dereference_census_actually_finds_dereferences() {
    let (conn, _guard) = applied().await;

    let found: std::collections::BTreeSet<String> = names(
        &conn,
        &format!(
            "SELECT DISTINCT p.proname || '.' || m[2] AS v \
             {DEREFERENCES_OF_TRIGGER_FUNCTIONS} \
             ORDER BY 1"
        ),
    )
    .await
    .into_iter()
    .collect();

    let missing: Vec<&&str> = REQUIRED_DEREFERENCES
        .iter()
        .filter(|name| !found.contains(**name))
        .collect();

    assert!(
        missing.is_empty(),
        "the extraction did not find {missing:?}; it has stopped matching, which would make the \
         census above vacuously green. It found {} dereferences in all",
        found.len()
    );
}

// ---------------------------------------------------------------------------
// Guards this chain declares per engine, executed on this one.
//
// The three cases below are the Postgres arms of refusals whose `SQLite` twins
// live in `tests/sqlite_migrations.rs`. Each is a **separate literal per engine**
// — a `CHECK` written twice, a PL/pgSQL arm against a `RAISE(ABORT)` trigger — so
// a green mirror is evidence about the mirror and nothing else.
// ---------------------------------------------------------------------------

/// Run one statement that must be refused, and hand back the server's words.
async fn must_be_refused(conn: &DatabaseConnection, sql: &str, because: &str) -> String {
    let message = conn
        .execute_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            sql.to_owned(),
        ))
        .await
        .err()
        .unwrap_or_else(|| panic!("this statement must be refused: {sql}"))
        .to_string();
    assert!(
        message.contains(because),
        "and refused by `{because}` rather than by a neighbouring guard: {message}"
    );
    message
}

/// **A negative add-on quantity bound is refused on this engine too.**
///
/// `chk_pricing_plan_addon_rule_qty_range` bounds only the *relation* between
/// `min_qty` and `max_qty`, so before the two lower bounds existed a negative in
/// either column was stored — and both read back as `Option<u32>`, where a
/// negative becomes `RepoError::CorruptRow` and a `500` over the whole revision's
/// add-on set. The constraint is declared once per engine, in two separate
/// literals, which is why the `SQLite` twin is not evidence for this one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_negative_add_on_quantity_bound_is_refused() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000b1";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let (conn, _guard) = applied().await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;

    let rule = |sku: &str, column: &str, value: &str| {
        format!(
            "INSERT INTO bss.pricing_plan_addon_rule \
             (plan_id, plan_revision, addon_sku_id, tenant_id, required, {column}) \
             VALUES ('{PLAN}', 0, '{sku}', '{TENANT}', false, {value})"
        )
    };

    must_be_refused(
        &conn,
        &rule("55555555-0000-0000-0000-0000000000b1", "min_qty", "-1"),
        "chk_pricing_plan_addon_rule_min_qty",
    )
    .await;
    // `max_qty` on a rule that is **not** required, because a required one is
    // already refused by section 6's own `>= 1` arm and would prove that instead.
    must_be_refused(
        &conn,
        &rule("55555555-0000-0000-0000-0000000000b2", "max_qty", "-1"),
        "chk_pricing_plan_addon_rule_max_qty",
    )
    .await;

    // Zero is a bound, not a violation, and the relation between the two columns
    // is still `_qty_range`'s to judge rather than something these two took over.
    must_succeed(
        &conn,
        &rule("55555555-0000-0000-0000-0000000000b3", "min_qty", "0"),
    )
    .await;
    must_be_refused(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan_addon_rule \
             (plan_id, plan_revision, addon_sku_id, tenant_id, required, min_qty, max_qty) \
             VALUES ('{PLAN}', 0, '55555555-0000-0000-0000-0000000000b4', '{TENANT}', false, 5, 2)"
        ),
        "chk_pricing_plan_addon_rule_qty_range",
    )
    .await;
}

/// **A journal row may not name a run belonging to another tenant.**
///
/// `fk_pricing_repricing_journal_run` covers `run_id` alone, and the arm that
/// compares the two tenants is one branch of a PL/pgSQL function here against a
/// standalone `RAISE(ABORT)` trigger on `SQLite` — the sibling table
/// `pricing_bulk_row_lock` splits the same rule the same way. Defence in depth
/// rather than a live hole: the only production writer opens the run and journals
/// its rows from one scope inside one transaction, which is the positive control
/// below.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_journal_row_may_not_name_another_tenants_run() {
    const TENANT_A: &str = "11111111-1111-1111-1111-111111111111";
    const TENANT_B: &str = "11111111-1111-1111-1111-1111111111b2";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000c1";
    const PHASE: &str = "33333333-0000-0000-0000-0000000000c1";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
    const PRICE: &str = "8f8f8f8f-0000-0000-0000-0000000000c1";
    const RUN: &str = "8f8f8f8f-0000-0000-0000-0000000000c2";

    let (conn, _guard) = applied().await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_price (price_id, tenant_id, plan_id, currency, region, \
             phase, charge_kind, model_kind, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PRICE}', '{TENANT_A}', '{PLAN}', 'USD', 'EU', '{PHASE}', 'usage', \
             'per_unit', 'draft', '{ACTOR}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_bulk_operation (operation_id, tenant_id, kind, state, \
             client_key, submitted_by, submitted_at) \
             VALUES ('{RUN}', '{TENANT_A}', 'repricing', 'validating', 'ck-b1-tenancy', \
             '{ACTOR}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;

    must_be_refused(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('{RUN}', '{PRICE}', '{TENANT_B}', 'pending')"
        ),
        "belongs to another tenant",
    )
    .await;

    // The control: the same row under the run's own tenant, which is the only
    // shape a request can produce.
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('{RUN}', '{PRICE}', '{TENANT_A}', 'pending')"
        ),
    )
    .await;
    // And the arm still defers to the foreign key for a row naming no run at all:
    // a `BEFORE` trigger answers ahead of the key, so an arm without its `FOUND`
    // conjunct would report a tenancy fault the caller does not have and leave the
    // key unobservable.
    must_be_refused(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_repricing_journal (run_id, price_id, tenant_id, state) \
             VALUES ('8f8f8f8f-0000-0000-0000-0000000000ff', '{PRICE}', '{TENANT_B}', 'pending')"
        ),
        "fk_pricing_repricing_journal_run",
    )
    .await;
}

/// Every character the blankness predicates strip, as a code point.
///
/// ASCII whitespace entire, the same set the `SQLite` arm strips. `ScopeValue::new`
/// is Rust's `str::trim`, which strips every character carrying the Unicode
/// `White_Space` property; what this set cannot reach is stated on
/// `pricing_region_taxonomy`'s migration.
const STRIPPED_WHITESPACE: &[u32] = &[9, 10, 11, 12, 13, 32];

/// A value that pads a real one. The control for every case below: the predicates
/// refuse a value with no non-blank character at all, never one that merely needs a
/// trim.
const PADDED: &str = " EU ";

/// **Every taxonomy's `value` predicate refuses ASCII whitespace alone on this
/// engine too** — D-242 (`pricing_region_taxonomy`).
///
/// The predicate is declared once per engine, in two separate literals with two
/// different spellings — `btrim(X, Y)` here against `SQLite`'s `trim(X, Y)` — so the
/// fast-tier twin is not evidence for this arm. One character per statement rather
/// than one mixed string: `btrim` takes a *set*, and a set that lost a member still
/// refuses a string holding the others.
///
/// What the row costs is one level over the store: `taxonomy_repo`'s readers map a
/// value `ScopeValue::new` refuses to `RepoError::CorruptRow`, so one such row fails
/// `GET` for **every** value in its class and the `PUT` cannot round-trip a list it
/// cannot read.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_taxonomy_value_predicate_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const TAXONOMIES: &[&str] = &[
        "pricing_brand_taxonomy",
        "pricing_customer_group_taxonomy",
        "pricing_org_tier_taxonomy",
        "pricing_partner_taxonomy",
        "pricing_region_taxonomy",
        "pricing_rounding_policy_taxonomy",
        "pricing_gl_code_taxonomy",
    ];

    let (conn, _guard) = applied().await;

    for table in TAXONOMIES {
        for code in STRIPPED_WHITESPACE {
            must_be_refused(
                &conn,
                &format!(
                    "INSERT INTO bss.{table} (tenant_id, value, display_name, state) \
                     VALUES ('{TENANT}', chr({code}), 'blank', 'active')"
                ),
                &format!("chk_{table}_value_present"),
            )
            .await;
        }

        must_succeed(
            &conn,
            &format!(
                "INSERT INTO bss.{table} (tenant_id, value, display_name, state) \
                 VALUES ('{TENANT}', '{PADDED}', 'padded', 'active')"
            ),
        )
        .await;
    }
}

/// **A composite's `output_unit` may not be ASCII whitespace alone on this engine
/// too** — `chk_pricing_composite_meter_output_unit`.
///
/// A unit of nothing but blanks renders on an invoice line as a blank and joins no
/// meter to any unit, and `uq_pricing_composite_meter_output` then holds it as if it
/// were a name — one blank unit per revision, reserved.
///
/// Pinned by the constraint's own name, because a table-name discriminator is shared
/// by every guard here — the draft-only arm, the missing-parent arm, the same-tenant
/// arm — and would pass for whichever one answered. The parent is a `draft` revision
/// of this tenant so that `pricing_composite_meter_append_only`, a `BEFORE` trigger
/// and therefore ahead of constraint checking, has nothing to say.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_composite_output_unit_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000c1";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let (conn, _guard) = applied().await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;

    let composite = |id: u32, unit: &str| {
        format!(
            "INSERT INTO bss.pricing_composite_meter (tenant_id, plan_id, plan_revision, \
             composite_id, constituent_units, formula, output_unit) \
             VALUES ('{TENANT}', '{PLAN}', 0, '{id:0>8}-0000-0000-0000-000000000000', \
             '[\"vcpu\"]'::jsonb, '{{\"op\":\"sum\"}}'::jsonb, {unit})"
        )
    };

    for code in STRIPPED_WHITESPACE {
        must_be_refused(
            &conn,
            &composite(*code, &format!("chr({code})")),
            "chk_pricing_composite_meter_output_unit",
        )
        .await;
    }

    must_succeed(&conn, &composite(99, &format!("'{PADDED}'"))).await;
}

/// **A membership's `group_value` may not be blank, of any width** —
/// `chk_pricing_group_membership_group_value_present`, on both engines and executed
/// on neither before.
///
/// The group value is the name `inst-cg-resolve` resolves a payer's price by, and
/// `required_group` mints the path segment through `ScopeValue::new`, which trims, so
/// a blank one is a group no writer in the gear can produce, no reader can address,
/// and nothing can tell from another blank. `length(group_value) > 0` admits a
/// **single space**, which is why the space is asserted alongside the rest of the
/// set.
///
/// One payer for every refused statement: none of them lands, so
/// `pricing_group_membership_no_overlap` has no interval to collide with and the
/// CHECK is what answers.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_group_membership_group_value_refuses_ascii_whitespace_alone() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PAYER: &str = "22222222-0000-0000-0000-0000000000d1";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let (conn, _guard) = applied().await;

    let membership = |id: u32, group_value: &str| {
        format!(
            "INSERT INTO bss.pricing_group_membership (membership_id, tenant_id, \
             payer_tenant_id, group_value, effective_from, effective_to, created_by, \
             created_at_utc) \
             VALUES ('{id:0>8}-0000-0000-0000-000000000000', '{TENANT}', '{PAYER}', \
             {group_value}, '2026-01-01 00:00:00+00', NULL, '{ACTOR}', \
             '2026-08-17 09:00:00+00')"
        )
    };

    for code in STRIPPED_WHITESPACE {
        must_be_refused(
            &conn,
            &membership(*code, &format!("chr({code})")),
            "chk_pricing_group_membership_group_value_present",
        )
        .await;
    }

    must_succeed(&conn, &membership(99, &format!("'{PADDED}'"))).await;
}

/// **A rev-share party is held to both of `Party::new`'s refusals on this engine
/// too** — `chk_pricing_bundle_revshare_party`.
///
/// The predicate is declared once per engine in two separate literals, so the
/// fast-tier twin is not evidence for this arm. Two clauses and the trim is
/// load-bearing in each: `length(party) > 0` admits a single space, and
/// `party <> 'platform'` compares the **stored** text, so `' platform '` satisfied it
/// while trimming to the sentinel — a party forging the token
/// `pricing_bundle_revshare_group` uses for D-07's default.
///
/// The padded sentinel is asserted separately from the widths because a trim on the
/// blankness clause alone leaves it admitted.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_revshare_party_predicate_refuses_a_blank_and_a_padded_sentinel() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000e1";
    const BUNDLE: &str = "55555555-0000-0000-0000-0000000000e1";
    const VENDOR: &str = "cccccccc-0000-0000-0000-0000000000e1";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let (conn, _guard) = applied().await;
    seed_revshare_group(&conn, PLAN, BUNDLE, VENDOR, TENANT, ACTOR).await;

    let party = |value: &str| {
        format!(
            "INSERT INTO bss.pricing_bundle_revshare (bundle_id, plan_revision, \
             vendor_sku_id, party, tenant_id, share_bp) \
             VALUES ('{BUNDLE}', 0, '{VENDOR}', {value}, '{TENANT}', 9000)"
        )
    };

    for code in STRIPPED_WHITESPACE {
        must_be_refused(
            &conn,
            &party(&format!("chr({code})")),
            "chk_pricing_bundle_revshare_party",
        )
        .await;
    }
    for forged in ["' platform '", "'platform '", "chr(9) || 'platform'"] {
        must_be_refused(&conn, &party(forged), "chk_pricing_bundle_revshare_party").await;
    }

    // The control: a padded party is a party — `Party::new` reads `' acme '` back as
    // `acme` — so the refusals above are about a value with nothing in it and about
    // the sentinel wearing padding, and not about padding as such.
    must_succeed(&conn, &party("' acme '")).await;
}

/// **The absorber predicate is `Absorber::parse`'s two arms on this engine too** —
/// `chk_pricing_bundle_revshare_group_absorber`.
///
/// The column holds the sentinel (D-07's default, so an unnominated state cannot
/// exist) or a party of the group, and `Absorber::parse` reads the sentinel by
/// equality **before** it tries `Party::new`. So `' platform '` falls through to
/// `Party::new`, which trims and refuses it for spelling the sentinel: a value that is
/// neither the default nor a nomination.
///
/// The sentinel's own row is the control that makes this falsifiable the other way: a
/// predicate that simply trimmed the column would refuse every unnominated group,
/// which is the default and the common case.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_absorber_predicate_refuses_a_blank_and_a_padded_sentinel() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000e2";
    const BUNDLE: &str = "55555555-0000-0000-0000-0000000000e2";
    const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

    let (conn, _guard) = applied().await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PLAN}', 0, '{TENANT}', 'draft', '{ACTOR}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_bundle \
             (bundle_id, tenant_id, plan_id, price_basis, invoice_itemization) \
             VALUES ('{BUNDLE}', '{TENANT}', '{PLAN}', 'sum_of_parts', 'aggregate')"
        ),
    )
    .await;

    let group = |vendor: u32, absorber: &str| {
        format!(
            "INSERT INTO bss.pricing_bundle_revshare_group (bundle_id, plan_revision, \
             vendor_sku_id, tenant_id, platform_cut_bp, residual_absorber_party) \
             VALUES ('{BUNDLE}', 0, '{vendor:0>8}-0000-0000-0000-0000000000e2', '{TENANT}', \
             1000, {absorber})"
        )
    };

    for code in STRIPPED_WHITESPACE {
        must_be_refused(
            &conn,
            &group(*code, &format!("chr({code})")),
            "chk_pricing_bundle_revshare_group_absorber",
        )
        .await;
    }
    for (vendor, forged) in [(90, "' platform '"), (91, "chr(9) || 'platform'")] {
        must_be_refused(
            &conn,
            &group(vendor, forged),
            "chk_pricing_bundle_revshare_group_absorber",
        )
        .await;
    }

    // Both legal inhabitants land: the sentinel exactly, and a named party.
    must_succeed(&conn, &group(92, "'platform'")).await;
    must_succeed(&conn, &group(93, "'acme'")).await;
}

/// **An overlay line's `target_sku` is absent or names something, on this engine
/// too** — `chk_pricing_price_overlay_line_target_sku_present`.
///
/// `NULL` and a blank string are not the same state and only one of them is a line:
/// the list-default and per-plan lines carry no SKU at all, while `TargetSku::new`
/// trims and `overlay_repo` folds its refusal to `RepoError::CorruptRow` over the
/// revision the row sits in. The `NULL` arm is asserted as its own control — a
/// tightening that turned an absent SKU into a refusal would break every line
/// `LineKey::list_default` and `LineKey::for_plan` build.
///
/// The plan is named on every row because `chk_..._sku_needs_plan` answers first
/// otherwise, and a mis-arranged fixture would prove that neighbouring rule twice and
/// leave this one untouched.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_target_sku_predicate_refuses_a_blank_and_keeps_its_null_arm() {
    const TENANT: &str = "11111111-1111-1111-1111-111111111111";
    const PLAN: &str = "22222222-0000-0000-0000-0000000000e3";
    const OVERLAY: &str = "66666666-0000-0000-0000-0000000000e3";

    let (conn, _guard) = applied().await;
    // A `draft` overlay revision of this tenant, or the line table's append-only and
    // same-tenant arms answer ahead of the CHECK.
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_price_overlay (tenant_id, price_overlay_id, revision, \
             lifecycle_state, precedence, scope_class, scope_value, tax_basis) \
             VALUES ('{TENANT}', '{OVERLAY}', 0, 'draft', 10, 'brand', 'acme', 'exclusive')"
        ),
    )
    .await;

    let line = |id: u32, sku: &str| {
        format!(
            "INSERT INTO bss.pricing_price_overlay_line (line_id, price_overlay_id, \
             overlay_revision, tenant_id, plan_id, target_sku, cohort, adjustment_kind, \
             magnitude_kind, adjustment_value) \
             VALUES ('{id:0>8}-0000-0000-0000-0000000000e3', '{OVERLAY}', 0, '{TENANT}', \
             '{PLAN}', {sku}, NULL, 'discount', 'percent_bp', 1500)"
        )
    };

    for code in STRIPPED_WHITESPACE {
        must_be_refused(
            &conn,
            &line(*code, &format!("chr({code})")),
            "chk_pricing_price_overlay_line_target_sku_present",
        )
        .await;
    }

    // The `NULL` arm, which is the whole reason this predicate is a disjunction, and a
    // named SKU beside it.
    must_succeed(&conn, &line(98, "NULL")).await;
    must_succeed(&conn, &line(99, "' vm-small '")).await;
}

/// A draft plan revision, its bundle and one rev-share group, for a party row to hang
/// off: the party table's foreign key and its append-only arm both resolve through
/// them and either would answer ahead of the CHECK under test.
async fn seed_revshare_group(
    conn: &DatabaseConnection,
    plan: &str,
    bundle: &str,
    vendor: &str,
    tenant: &str,
    actor: &str,
) {
    must_succeed(
        conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{plan}', 0, '{tenant}', 'draft', '{actor}', '2026-08-17 09:00:00+00')"
        ),
    )
    .await;
    must_succeed(
        conn,
        &format!(
            "INSERT INTO bss.pricing_bundle \
             (bundle_id, tenant_id, plan_id, price_basis, invoice_itemization) \
             VALUES ('{bundle}', '{tenant}', '{plan}', 'sum_of_parts', 'aggregate')"
        ),
    )
    .await;
    must_succeed(
        conn,
        &format!(
            "INSERT INTO bss.pricing_bundle_revshare_group (bundle_id, plan_revision, \
             vendor_sku_id, tenant_id, platform_cut_bp, residual_absorber_party) \
             VALUES ('{bundle}', 0, '{vendor}', '{tenant}', 1000, 'platform')"
        ),
    )
    .await;
}
