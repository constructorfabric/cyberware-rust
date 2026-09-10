//! `pricing_plan_phase`, `pricing_plan_addon_rule`,
//! `pricing_plan_descriptor_set` and `pricing_plan_period_floor_cap` — the four
//! revision-scoped children of a plan revision — proved by **executing the
//! statement each object must refuse**, on Postgres.
//!
//! # Why this suite exists
//!
//! `pricing_plan_phase`, `…000013` and `…000014` had never run on the backend they
//! target. `tests/sqlite_plan_phase.rs`, `tests/sqlite_plan_addon_rule.rs` and
//! `tests/sqlite_plan_descriptor_set.rs` prove the **mirror**, which is a
//! different set of objects: three fixed-message `RAISE(ABORT, …)` triggers per
//! table where Postgres has one PL/pgSQL function with two arms, and a `WHERE
//! NOT EXISTS` subquery in the trigger body where Postgres does a `SELECT …
//! INTO`. They also reach the tables through repositories, which is the layer
//! that cannot see a guard stop refusing.
//!
//! `tests/postgres_migrations.rs` closed half the remaining gap by pinning the
//! CHECK, trigger and partial-index rosters **by name**, so an object cannot
//! vanish unnoticed. It issues no DML, so it says the objects reached the server
//! and nothing about what any of them does. This suite is the other half for
//! these three tables: one executed refusal per object, and the assertion names
//! the object the refusal came from.
//!
//! # The three rules every test here follows
//!
//! **Execute the refusal.** A test that writes valid values is not evidence
//! about a guard. It catches a constraint that got *narrower* — the writer
//! starts failing — and never one that stopped refusing.
//!
//! **Put the world in the state where the object under test is what answers.**
//! Three separate hazards live on these tables and each of them was hit while
//! writing this file:
//!
//! * `uq_pricing_plan_phase_terminal` refuses a second phase with a NULL
//!   `converts_to_phase_id` under one revision, so every phase row below that is
//!   not *about* that index carries a successor and stays outside its predicate.
//!   Without that, the second `INSERT` of any multi-row test is answered by the
//!   index rather than by the constraint under test.
//! * `uq_pricing_plan_open_draft` on the **parent** refuses a second `draft`
//!   revision of one plan, so every draft parent here is a distinct plan.
//! * `chk_pricing_plan_addon_rule_required_max_qty` and
//!   `chk_pricing_plan_addon_rule_qty_range` overlap on a row that is both
//!   `required` and inverted, so each refusal moves exactly one column of an
//!   otherwise-valid row.
//!
//! **Assert the object, never the table.** Every CHECK, index, foreign key and
//! trigger message over these tables carries the table name, as does the column
//! list Postgres prints for a unique violation. A test that accepted any error
//! naming the table would pass with the guard it means to prove switched off.
//!
//! # The trigger has two arms and they overlap almost completely
//!
//! All three tables carry the same function shape:
//!
//! ```text
//! IF TG_OP <> 'INSERT' THEN  -- arm 1: the OLD parent must be a draft
//! IF TG_OP =  'DELETE' THEN RETURN OLD;
//!                            -- arm 2: the NEW parent must be a draft
//! ```
//!
//! On an ordinary `UPDATE` — one that leaves `plan_id` and `plan_revision`
//! alone — the OLD parent and the NEW parent are the **same row**, so arm 2
//! refuses everything arm 1 refuses and raises the identical sentence. Arm 1's
//! only unshared statement is therefore the **DELETE**, which arm 2 never sees
//! (it returns `OLD` above the second lookup). Arm 2's unshared statements are
//! the **INSERT** and the `UPDATE` that **re-points** a child row from a draft
//! revision onto a frozen one — the statement by which a frozen revision would
//! otherwise acquire a child without an INSERT ever being issued.
//!
//! The tests are split along exactly that line, so deleting either arm reddens
//! exactly one of them. The ordinary frozen-parent `UPDATE` is kept with arm 1,
//! which is what answers it in the intact schema; its assertion survives arm 1's
//! deletion (arm 2 catches it) and the DELETEs beside it do not.
//!
//! ## The missing-parent branch is shadowed by the foreign key
//!
//! Arm 2's `coalesce(parent_state, 'missing')` fires on an INSERT naming a
//! revision that does not exist — but so would
//! `fk_pricing_plan_phase_revision`. The trigger is `BEFORE` and the FK is
//! checked at end of statement, so the trigger is what *answers*; nothing is
//! reachable through that branch alone. It is exercised for the message rather
//! than claimed as a guard.
//!
//! ## And the foreign key is shadowed on every INSERT
//!
//! The mirror image: a child INSERT passes arm 2 only if the parent row exists
//! **and** reads `draft`, so by the time the FK is consulted its referent is
//! always there. The only statement the FK alone can refuse is a **parent** that
//! moves out from under its children — `UPDATE pricing_plan SET revision = …` on
//! a draft revision, which the parent's own trigger waves through because the
//! draft plane is where content moves. That is the statement each FK test below
//! executes.
//!
//! # Positives are load-bearing
//!
//! Every guard here is a whitelist rather than a blanket ban, so the suite
//! carries the accepting cases: all three phase kinds store, a revision holds
//! many phases beside its one terminal, a revision holds many add-on rules
//! (D-105's whole point), a descriptor set stores with every optional column
//! NULL, two plans — and two tenants — hold one phase id (D-340), and INSERT,
//! UPDATE and DELETE all run freely under a `draft` parent. Without those a table
//! nothing can be written to at all would pass.
//!
//! # Objects this suite deliberately does not test by refusal
//!
//! `idx_pricing_plan_phase_revision`, `idx_pricing_plan_addon_rule_revision` and
//! `idx_pricing_plan_descriptor_set_revision` are **non-unique, non-partial**
//! indexes. They refuse nothing; their only observable effect is on plan choice,
//! which is not a correctness property and would make a brittle test. Their
//! presence is pinned by name in `tests/postgres_migrations.rs`, and that is the
//! whole of what can be said about them here.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_schema_plan_shape -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use pg_support::Pg;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

/// One parent plan per state a test needs to reach.
///
/// `uq_pricing_plan_open_draft` is keyed on `plan_id` alone, so two draft
/// revisions cannot share a plan; every draft parent here is therefore its own
/// plan. The frozen parents are reached by seeding a draft and taking a
/// sanctioned flip, because a child cannot be inserted under a frozen revision
/// at all — which is the very thing arm 2 exists to say.
const PLAN_A: &str = "22222222-0000-0000-0000-00000000000a";
const PLAN_B: &str = "22222222-0000-0000-0000-00000000000b";
const PLAN_C: &str = "22222222-0000-0000-0000-00000000000c";
const PLAN_D: &str = "22222222-0000-0000-0000-00000000000d";

/// A plan id no `pricing_plan` row carries, for the missing-parent branch.
const PLAN_ABSENT: &str = "22222222-0000-0000-0000-0000000000ff";

/// A second tenant, for the guards on these tables that are statements about
/// **whose** rows they are: `pricing_plan_phase`'s key (D-340) and the
/// append-only trigger's parent-tenant arm.
const TENANT_TWO: &str = "11111111-0000-0000-0000-0000000000b2";

/// The phase id the stand's five drafts share, used verbatim: what D-340 measured
/// is that four of those five could never attach *this* id.
const SHARED_PHASE: &str = "1e256059-2956-4b3e-8acd-898eb43a1ff7";

const PHASE_1: &str = "33333333-0000-0000-0000-000000000001";
const PHASE_2: &str = "33333333-0000-0000-0000-000000000002";
const PHASE_3: &str = "33333333-0000-0000-0000-000000000003";
/// A successor id, so a phase row stays **outside**
/// `uq_pricing_plan_phase_terminal`'s predicate. There is deliberately no FK on
/// `converts_to_phase_id` (the migration's module doc argues it), so this need
/// not name a stored row.
const SUCCESSOR: &str = "33333333-0000-0000-0000-0000000000aa";

const ADDON_1: &str = "55555555-0000-0000-0000-000000000001";
const ADDON_2: &str = "55555555-0000-0000-0000-000000000002";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A fresh database carrying the applied chain, on the one shared server.
///
/// **One** container for the whole binary and a `CREATE DATABASE` per test; the
/// arrangement and the eleven false positives that motivated it are documented
/// in `tests/pg_support/mod.rs`.
///
/// The connection handed back is a **plain** one: every statement this suite
/// issues is raw SQL that deliberately reaches past `PlanShapeRepo`, because the
/// repository is exactly the layer that cannot see a guard stop refusing.
async fn applied() -> DatabaseConnection {
    Pg::applied().await.raw().await
}

async fn exec(conn: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    conn.execute_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        sql.to_owned(),
    ))
    .await
    .map(|_| ())
}

/// Run one statement that must land.
async fn must_succeed(conn: &DatabaseConnection, sql: &str) {
    exec(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("statement must succeed: {sql}\n{e}"));
}

/// Reject, **and by the named object**.
///
/// See the module doc: the fragment is the whole assertion, because every guard
/// over these tables names the table too.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, by: &str) {
    let err = exec(conn, sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the guard `{by}` must reject: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains(by),
        "the rejection must be the one under test (`{by}`), got: {message}"
    );
}

// ---------------------------------------------------------------------------
// Parents
// ---------------------------------------------------------------------------

/// Seed revision 0 of a plan as an open `draft` — the only state a child row can
/// be created under.
async fn seed_draft(conn: &DatabaseConnection, plan: &str) {
    seed_draft_for(conn, TENANT, plan).await;
}

/// The same, under a named tenant.
///
/// One tenant is enough for every guard on these tables except the key's scope,
/// which is a statement about what belongs to whom (D-340).
async fn seed_draft_for(conn: &DatabaseConnection, tenant: &str, plan: &str) {
    must_succeed(
        conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{plan}', 0, '{tenant}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
}

/// Take a sanctioned flip out of `draft`, freezing the revision **and its
/// children** without touching the children.
///
/// This is how a child row comes to sit under a frozen parent at all: arm 2
/// refuses the INSERT if the parent is already frozen, so the row has to be
/// written first and the parent frozen afterwards — which is exactly the
/// production ordering (`PlanRepo::commit` flips the revision last).
async fn freeze(conn: &DatabaseConnection, plan: &str, state: &str) {
    must_succeed(
        conn,
        &format!(
            "UPDATE bss.pricing_plan SET lifecycle_state = '{state}' \
             WHERE plan_id = '{plan}' AND revision = 0"
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — row builder
// ---------------------------------------------------------------------------

/// A minimal **valid** non-terminal phase of revision 0 of `plan`.
///
/// `converts_to_phase_id` is set rather than NULL on purpose: a NULL successor
/// puts the row inside `uq_pricing_plan_phase_terminal`, and a second such row
/// under one revision is refused by the index before any constraint under test
/// is consulted.
fn phase_row(phase: &str, plan: &str) -> Vec<(String, String)> {
    [
        ("phase_id", format!("'{phase}'")),
        ("plan_revision", "0".to_owned()),
        ("tenant_id", format!("'{TENANT}'")),
        ("plan_id", format!("'{plan}'")),
        ("kind", "'evergreen'".to_owned()),
        ("ordinal", "0".to_owned()),
        ("converts_to_phase_id", format!("'{SUCCESSOR}'")),
    ]
    .into_iter()
    .map(|(column, value)| (column.to_owned(), value))
    .collect()
}

fn render(table: &str, columns: &[(String, String)]) -> String {
    let names = columns
        .iter()
        .map(|(column, _)| column.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let values = columns
        .iter()
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO bss.{table} ({names}) VALUES ({values})")
}

fn with_overrides(
    mut columns: Vec<(String, String)>,
    overrides: &[(&str, &str)],
) -> Vec<(String, String)> {
    for (name, value) in overrides {
        match columns.iter_mut().find(|(column, _)| column == name) {
            Some(slot) => (*value).clone_into(&mut slot.1),
            None => columns.push(((*name).to_owned(), (*value).to_owned())),
        }
    }
    columns
}

/// `INSERT` of [`phase_row`] with the named columns replaced or added.
fn insert_phase(phase: &str, plan: &str, overrides: &[(&str, &str)]) -> String {
    render(
        "pricing_plan_phase",
        &with_overrides(phase_row(phase, plan), overrides),
    )
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — the world it accepts
// ---------------------------------------------------------------------------

/// The valid rows, first. Without this every refusal below would pass against a
/// table that refuses everything.
///
/// One per `kind` token, because `chk_pricing_plan_phase_kind` admits three and
/// a suite that only inserted `evergreen` would leave two thirds of the admitted
/// set unexercised. All three carry a successor, so the terminal index is not
/// what admits or refuses them.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn every_phase_kind_the_domain_renders_is_storable() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("kind", "'trial'")]),
    )
    .await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_2, PLAN_A, &[("kind", "'interim'")]),
    )
    .await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_3, PLAN_A, &[("kind", "'evergreen'")]),
    )
    .await;
}

/// The operator label (D-357) is stored as given and may be `NULL` — the
/// unlabelled state every phase authored before D-357 is in.
///
/// Both arms, because a column added by editing the migration in place is the
/// kind of change a fresh chain shows and an applied stand does not: this is the
/// fresh chain's half of the proof, and D-358's runbook is the stand's.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_label_is_stored_as_given_and_may_be_null() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(
        &conn,
        &insert_phase(
            PHASE_1,
            PLAN_A,
            &[("kind", "'interim'"), ("display_name", "'Onboarding'")],
        ),
    )
    .await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_2, PLAN_A, &[("display_name", "NULL")]),
    )
    .await;
    let stored = conn
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT phase_id::text AS phase_id, display_name FROM bss.pricing_plan_phase \
                 WHERE plan_id = '{PLAN_A}' ORDER BY phase_id"
            ),
        ))
        .await
        .expect("read the labels back");
    let labels: Vec<(String, Option<String>)> = stored
        .iter()
        .map(|row| {
            (
                row.try_get::<String>("", "phase_id").expect("phase_id"),
                row.try_get::<Option<String>>("", "display_name")
                    .expect("display_name"),
            )
        })
        .collect();
    assert_eq!(
        labels,
        [
            (PHASE_1.to_owned(), Some("Onboarding".to_owned())),
            (PHASE_2.to_owned(), None),
        ],
        "the label is stored as given, and its absence is NULL rather than an empty string"
    );
}

/// A draft revision's phase rows are freely mutable **and deletable**, which is
/// what makes the trigger a whitelist rather than a freeze.
///
/// The DELETE is the load-bearing half: D-145 has `PlanRepo::abandon_draft` drop
/// a discarded revision's child copies, and a table whose rows could never be
/// deleted would make that path unbuildable while every refusal test below
/// stayed green.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_draft_revisions_phases_are_insertable_mutable_and_deletable() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_phase(PHASE_1, PLAN_A, &[])).await;
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_phase SET kind = 'interim', ordinal = 3, \
             phase_duration_days = 30 WHERE phase_id = '{PHASE_1}' AND plan_revision = 0"
        ),
    )
    .await;
    // Re-pointing a child onto **another draft** revision is legal: both ends of
    // the trigger's question answer `draft`. It is the same statement shape the
    // arm-2 test uses against a frozen target, and having it here keeps that
    // refusal a fact about the parent's state rather than about the verb.
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_phase SET plan_id = '{PLAN_B}' \
             WHERE phase_id = '{PHASE_1}' AND plan_revision = 0"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_phase WHERE phase_id = '{PHASE_1}'"),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — the two CHECK constraints
// ---------------------------------------------------------------------------

/// The three tokens `domain::plan_shape::PhaseKind` renders, and nothing else.
///
/// A near-miss token stored here reads back as a corrupt row through every typed
/// path, and `TERMINAL_PHASE_KIND_INVALID` — the pipeline rule that pairs
/// terminality with `evergreen` — is written over exactly this vocabulary.
///
/// `intro` is in the list on purpose: it was the middle token until D-358, and
/// the rename is not real until the store refuses the old spelling — a `CHECK`
/// that still admitted it would let a pre-rename writer store a row every typed
/// reader answers `CorruptRow` for.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_kind_outside_the_three_is_refused() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    for kind in ["'promo'", "'EVERGREEN'", "'standard'", "'intro'"] {
        must_be_rejected(
            &conn,
            &insert_phase(PHASE_1, PLAN_A, &[("kind", kind)]),
            "chk_pricing_plan_phase_kind",
        )
        .await;
    }
}

/// Neither day-count column admits a negative.
///
/// Both are read back through `u32::try_from` and answer `CorruptRow` for
/// anything else, so a poisoned row is a revision no typed path reads at all.
///
/// **Each is reachable alone**, which is what keeps the two cases apart:
/// `chk_pricing_plan_phase_display_trial_days` is satisfied whenever either side
/// is NULL, so a duration of `-1` under an untaken projection and a projection of
/// `-1` over an absent duration each leave one constraint able to answer.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn neither_day_count_column_admits_a_negative() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("phase_duration_days", "-1")]),
        "chk_pricing_plan_phase_duration_non_negative",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("display_trial_days", "-1")]),
        "chk_pricing_plan_phase_trial_projection_non_negative",
    )
    .await;
    // Zero days is a real phase — one that converts the moment it opens — so
    // neither bound is `> 0`.
    must_succeed(
        &conn,
        &insert_phase(
            PHASE_1,
            PLAN_A,
            &[("phase_duration_days", "0"), ("display_trial_days", "0")],
        ),
    )
    .await;
}

/// The persisted projection may not drift from its source (`inst-ph-trial`).
///
/// Subscriptions reads the published `displayTrialDays` as its single source for
/// trial runtime, so a drift here is a trial that ends on a different day than
/// the catalog says it does.
///
/// The accepting cases are in the same test rather than a sibling because they
/// are the same fact seen from the other side, and the second of them is the
/// **hole the migration's own doc argues for**: a `display_trial_days` set
/// against a NULL `phase_duration_days` makes the comparison NULL, which both
/// engines count as satisfied. The constraint's name promises more than the
/// constraint delivers, and a reader who trusted the name would believe that
/// shape unstorable. It is not; publish refuses it instead, as
/// `PHASE_DURATION_INVALID` or `TERMINAL_PHASE_KIND_INVALID`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_display_trial_days_that_disagrees_with_its_duration_is_refused() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_be_rejected(
        &conn,
        &insert_phase(
            PHASE_1,
            PLAN_A,
            &[
                ("kind", "'trial'"),
                ("phase_duration_days", "30"),
                ("display_trial_days", "7"),
            ],
        ),
        "chk_pricing_plan_phase_display_trial_days",
    )
    .await;
    // Agreeing is what the constraint admits.
    must_succeed(
        &conn,
        &insert_phase(
            PHASE_1,
            PLAN_A,
            &[
                ("kind", "'trial'"),
                ("phase_duration_days", "14"),
                ("display_trial_days", "14"),
            ],
        ),
    )
    .await;
    // An untaken projection.
    must_succeed(
        &conn,
        &insert_phase(PHASE_2, PLAN_A, &[("phase_duration_days", "14")]),
    )
    .await;
    // And the NULL hole, pinned so that closing it is a decision rather than an
    // accident.
    must_succeed(
        &conn,
        &insert_phase(PHASE_3, PLAN_A, &[("display_trial_days", "7")]),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — the parent revision's tenancy
// ---------------------------------------------------------------------------

/// A child row belongs to the tenant its own parent revision belongs to.
///
/// `fk_pricing_plan_phase_revision` covers `(plan_id, plan_revision)` alone,
/// while the table is `#[secure(tenant_col = "tenant_id")]` — so without the
/// trigger's parent-tenant arm the key admits a row naming somebody else's
/// tenant, invisible to every scoped reader and frozen with the revision it was
/// written under. `INSERT` and `UPDATE` both, because the arm sits on the
/// function's `NEW` path and a draft revision's phase is mutable.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_may_not_claim_a_tenant_its_revision_does_not_belong_to() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;

    must_be_rejected(
        &conn,
        &insert_phase(
            PHASE_1,
            PLAN_A,
            &[("tenant_id", &format!("'{TENANT_TWO}'"))],
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;

    // The same row under the revision's own tenant lands, so the refusal above is
    // about the tenant and about nothing else the row carries.
    must_succeed(&conn, &insert_phase(PHASE_1, PLAN_A, &[])).await;

    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_phase SET tenant_id = '{TENANT_TWO}' \
             WHERE phase_id = '{PHASE_1}'"
        ),
        "belongs to another tenant and may not hold this row",
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — the partial UNIQUE index
// ---------------------------------------------------------------------------

/// At most one **terminal** phase per revision — the half of `inst-ph-graph` an
/// index can see.
///
/// The other half, that there is at least one, is the pipeline's
/// (`PHASE_GRAPH_INVALID`): "there is no such row" is not a row, and an index
/// cannot range over it. Nobody should later try to strengthen this index into
/// the whole rule.
///
/// The accepting case is the point of the predicate: a chain of any length
/// coexists with its single terminal, and an index over
/// `(plan_id, plan_revision)` with the `WHERE` dropped would refuse the second
/// phase of every plan ever authored.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_terminal_phases_of_one_revision_cannot_coexist() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("converts_to_phase_id", "NULL")]),
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_2, PLAN_A, &[("converts_to_phase_id", "NULL")]),
        "uq_pricing_plan_phase_terminal",
    )
    .await;
    // A chain beside its one terminal is the shape the predicate exists to
    // admit.
    must_succeed(&conn, &insert_phase(PHASE_2, PLAN_A, &[])).await;
    must_succeed(&conn, &insert_phase(PHASE_3, PLAN_A, &[])).await;
    // And another revision has a terminal of its own.
    seed_draft(&conn, PLAN_B).await;
    must_succeed(
        &conn,
        &insert_phase(
            "33333333-0000-0000-0000-0000000000b1",
            PLAN_B,
            &[("converts_to_phase_id", "NULL")],
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_phase` — the primary key and the foreign key
// ---------------------------------------------------------------------------

/// `(tenant_id, plan_id, plan_revision, phase_id)` — the `plan_revision` half D-83
/// needs, the `phase_id` half D-19 forbids re-minting, and the two scope columns
/// **D-340** added (`pricing_plan_phase`).
///
/// The accepting case is the load-bearing one: **the same `phase_id` under a
/// different revision is a different row**, which is the whole of the
/// copy-forward. A key of `phase_id` alone would refuse it and the `phase` axis
/// of the canonical scope key (D-19) would have to re-mint on every revision,
/// moving every continuing price row onto a key nothing else is filed under.
///
/// The refusing case is what the widening deliberately **kept**: one plan's
/// revision still may not hold one phase id twice, because `inst-ph-graph` walks a
/// chain in which a phase id names at most one row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn one_phase_id_is_one_row_per_revision_and_no_more() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_phase(PHASE_1, PLAN_A, &[])).await;
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("ordinal", "9")]),
        "pricing_plan_phase_pkey",
    )
    .await;
    // The same phase carried forward onto revision 1 of the same plan. The
    // parent revision has to exist for the FK, and only one of a plan's
    // revisions may be a draft, so the predecessor publishes first — which is
    // the production ordering too.
    freeze(&conn, PLAN_A, "published").await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PLAN_A}', 1, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[("plan_revision", "1")]),
    )
    .await;
}

/// **Two plans may hold the same phase id — one tenant's two, and two tenants'
/// (D-340).**
///
/// The old key named neither `tenant_id` nor `plan_id`, so a phase id belonged to
/// one plan per revision *number* across the whole table. Measured on the stand
/// 2026-08-17: five drafts key price rows on [`SHARED_PHASE`] and exactly one of
/// them could attach it, the other four unrecoverably — a scope key is a price
/// row's identity and cannot be re-pointed. The cross-tenant arm was worse than a
/// denial: the difference between the refusal and the acceptance answered *is this
/// id in use somewhere I cannot read*.
///
/// Executed on Postgres because the key is a **server** object here — `SQLite`
/// spells it inside the `CREATE TABLE` and reaches it only by rebuilding the
/// table, so the two engines' arms of `pricing_plan_phase` are different statements
/// and the mirror's green says nothing about this one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn two_plans_may_hold_the_same_phase_id_across_plans_and_across_tenants() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    seed_draft(&conn, PLAN_B).await;
    seed_draft_for(&conn, TENANT_TWO, PLAN_C).await;

    must_succeed(&conn, &insert_phase(SHARED_PHASE, PLAN_A, &[])).await;
    must_succeed(&conn, &insert_phase(SHARED_PHASE, PLAN_B, &[])).await;
    must_succeed(
        &conn,
        &insert_phase(
            SHARED_PHASE,
            PLAN_C,
            &[("tenant_id", &format!("'{TENANT_TWO}'"))],
        ),
    )
    .await;

    // Counted back, because three statements that all landed prove nothing if the
    // table kept one row: `INSERT` is not `UPSERT` here, but the assertion should
    // not depend on that.
    let held = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n FROM bss.pricing_plan_phase \
                 WHERE phase_id = '{SHARED_PHASE}'"
            ),
        ))
        .await
        .expect("run the count")
        .expect("the count returns a row")
        .try_get::<i64>("", "n")
        .expect("read the count");
    assert_eq!(held, 3, "all three plans must hold the id");
}

/// A phase row may not outlive the revision it names.
///
/// The statement is a **parent** move and not a child insert, and the module doc
/// says why: every child INSERT that reaches the FK has already satisfied arm 2,
/// which required the parent row to exist. What the FK alone can refuse is the
/// draft revision renumbering itself out from under its children — legal as far
/// as `pricing_plan`'s own trigger is concerned, because the draft plane is
/// where content moves.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_row_may_not_be_orphaned_by_renumbering_its_revision() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_phase(PHASE_1, PLAN_A, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan SET revision = 5 \
             WHERE plan_id = '{PLAN_A}' AND revision = 0"
        ),
        "fk_pricing_plan_phase_revision",
    )
    .await;
    // The same statement on a childless draft revision is fine, so the refusal
    // above is about the child and not about renumbering.
    seed_draft(&conn, PLAN_B).await;
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan SET revision = 5 \
             WHERE plan_id = '{PLAN_B}' AND revision = 0"
        ),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `bss.pricing_plan_phase_append_only()` — arm 1, the OLD parent
// ---------------------------------------------------------------------------

/// A phase of a frozen revision cannot be deleted, in any of the four non-draft
/// states.
///
/// This is arm 1's unshared statement and the module doc says why: on an
/// ordinary UPDATE the OLD parent and the NEW parent are the same row, so arm 2
/// raises the same sentence about it. DELETE is the verb arm 2 never sees.
///
/// The four states are exercised together rather than in four siblings, and that
/// is a deliberate consequence of the guard-by-removal discipline: they are one
/// fact about **one** arm, and deleting the arm has to redden exactly one test.
/// The coverage is not decoration — the arm's condition is "not a draft", which
/// is four states and not one, and `abandoned` in particular is the state
/// `PlanRepo::abandon_draft` has to order around: D-145 drops a discarded
/// revision's phases, and the moment the parent reads `abandoned` this arm
/// refuses the DELETE.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_of_a_frozen_revision_cannot_be_deleted() {
    let conn = applied().await;
    // A phase id per parent, which the key no longer forces: since D-340 it is
    // `(tenant_id, plan_id, plan_revision, phase_id)`, so these four plans could
    // share an id. Kept distinct anyway — each `must_be_rejected` below names the
    // row it deleted by `phase_id` alone, and four rows under one id would leave
    // the four assertions unable to say which parent answered.
    for (plan, phase, state) in [
        (PLAN_A, PHASE_1, "published"),
        (PLAN_B, PHASE_2, "abandoned"),
        (PLAN_C, PHASE_3, "superseded"),
        (PLAN_D, "33333333-0000-0000-0000-000000000004", "retired"),
    ] {
        seed_draft(&conn, plan).await;
        must_succeed(&conn, &insert_phase(phase, plan, &[])).await;
        if state == "superseded" || state == "retired" {
            // The only edge into either is from `published`.
            freeze(&conn, plan, "published").await;
        }
        freeze(&conn, plan, state).await;
        must_be_rejected(
            &conn,
            &format!(
                "DELETE FROM bss.pricing_plan_phase \
                 WHERE phase_id = '{phase}' AND plan_id = '{plan}'"
            ),
            &format!(
                "pricing_plan_phase: DELETE of a phase under a {state} plan revision is not permitted"
            ),
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// `bss.pricing_plan_phase_append_only()` — arm 2, the NEW parent
// ---------------------------------------------------------------------------

/// A frozen revision acquires no phases — neither by INSERT nor by a child row
/// walking onto it.
///
/// Both statements here are arm 2's alone. The INSERT is one arm 1 never sees;
/// the re-pointing UPDATE passes arm 1 (its OLD parent is a draft) and is caught
/// only by the second lookup, and it is the realistic defect shape: it is how a
/// frozen `CatalogVersion` would acquire content without an INSERT ever being
/// issued, and the projector's warm re-drive reads truth rows (§4.4), so the
/// frozen version would silently re-materialize at a different shape.
///
/// The missing-parent case is exercised for its message and **not** claimed as a
/// guard: `fk_pricing_plan_phase_revision` would refuse it too, later in the
/// statement. See the module doc.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_frozen_revision_acquires_no_phases() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_1, PLAN_A, &[]),
        "pricing_plan_phase: INSERT of a phase under a published plan revision is not permitted",
    )
    .await;

    // The re-point: a live child of a draft revision, walked onto the frozen one.
    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_phase(PHASE_2, PLAN_B, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_phase SET plan_id = '{PLAN_A}' \
             WHERE phase_id = '{PHASE_2}' AND plan_revision = 0"
        ),
        "pricing_plan_phase: UPDATE of a phase under a published plan revision is not permitted",
    )
    .await;

    // And the branch with no parent at all, for its wording.
    must_be_rejected(
        &conn,
        &insert_phase(PHASE_3, PLAN_ABSENT, &[]),
        "pricing_plan_phase: INSERT of a phase under a missing plan revision is not permitted",
    )
    .await;
}

/// The ordinary frozen-parent UPDATE, which arm 1 answers in the intact schema.
///
/// It lives beside arm 1's DELETE test in spirit but in its own test in fact,
/// because it is the one statement **both** arms refuse: deleting either leaves
/// it refused by the other. It is here so that the behaviour is pinned at all —
/// a suite that never issued it would say nothing about the ordinary case — and
/// its doc says plainly that it is not evidence about either arm alone.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_phase_of_a_frozen_revision_cannot_be_edited_in_place() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_phase(PHASE_1, PLAN_A, &[])).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_phase SET ordinal = 9 \
             WHERE phase_id = '{PHASE_1}' AND plan_revision = 0"
        ),
        "pricing_plan_phase: UPDATE of a phase under a published plan revision is not permitted",
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_addon_rule` — row builder
// ---------------------------------------------------------------------------

/// A minimal **valid** add-on rule of revision 0 of `plan`: optional, unbounded,
/// no edges.
fn addon_row(addon: &str, plan: &str) -> Vec<(String, String)> {
    [
        ("plan_id", format!("'{plan}'")),
        ("plan_revision", "0".to_owned()),
        ("addon_sku_id", format!("'{addon}'")),
        ("tenant_id", format!("'{TENANT}'")),
    ]
    .into_iter()
    .map(|(column, value)| (column.to_owned(), value))
    .collect()
}

fn insert_addon(addon: &str, plan: &str, overrides: &[(&str, &str)]) -> String {
    render(
        "pricing_plan_addon_rule",
        &with_overrides(addon_row(addon, plan), overrides),
    )
}

// ---------------------------------------------------------------------------
// `pricing_plan_addon_rule` — the world it accepts
// ---------------------------------------------------------------------------

/// **D-105's whole point, from the accepting side.**
///
/// The earlier spelling keyed this table `(plan_id, plan_revision)`, which admits
/// one rule per revision — and three rules of this slice are written over pairs:
/// the `depends_on` cycle walk needs an edge, the symmetric-conflict
/// normalization needs a back-edge to land on, and "two required conflicting
/// add-ons fail publish" names two rows. None of them would have failed a test,
/// because a plan could never reach the state they reject.
///
/// Two rules under one revision is therefore the load-bearing positive of this
/// table, and the second half of the test is the primary key saying that
/// `addon_sku_id` is what distinguishes them.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn one_revision_holds_many_add_on_rules_distinguished_by_sku() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(
        &conn,
        &insert_addon(
            ADDON_1,
            PLAN_A,
            &[
                ("required", "true"),
                ("min_qty", "1"),
                ("max_qty", "5"),
                ("step_qty", "1"),
                ("conflicts_with_addon_sku_id", &format!("'[\"{ADDON_2}\"]'")),
            ],
        ),
    )
    .await;
    must_succeed(
        &conn,
        &insert_addon(
            ADDON_2,
            PLAN_A,
            &[("depends_on_addon_sku_id", &format!("'[\"{ADDON_1}\"]'"))],
        ),
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_addon(ADDON_2, PLAN_A, &[("required", "true"), ("max_qty", "2")]),
        "pricing_plan_addon_rule_pkey",
    )
    .await;
}

/// A draft revision's add-on rules are freely mutable and deletable, for the same
/// reason the phase rows are: `PlanRepo::abandon_draft` drops them.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_draft_revisions_add_on_rules_are_mutable_and_deletable() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_A, &[])).await;
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_addon_rule SET min_qty = 2, max_qty = 8 \
             WHERE addon_sku_id = '{ADDON_1}'"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_addon_rule WHERE addon_sku_id = '{ADDON_1}'"),
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_addon_rule` — the three CHECK constraints
// ---------------------------------------------------------------------------

/// §6 verbatim: a **required** add-on must admit at least one unit.
///
/// A required add-on with no upper bound at all, or with a bound of zero, is a
/// plan that is sellable and unbuyable. Both shapes are here because they are the
/// constraint's two conjuncts and a version that dropped either would stay green
/// against the other's case.
///
/// The row is otherwise valid in the *other* two constraints' terms —
/// `min_qty` stays NULL, so `chk_…_qty_range` is satisfied and cannot be what
/// answers.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_required_add_on_that_admits_no_quantity_is_refused() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_be_rejected(
        &conn,
        &insert_addon(ADDON_1, PLAN_A, &[("required", "true")]),
        "chk_pricing_plan_addon_rule_required_max_qty",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_addon(ADDON_1, PLAN_A, &[("required", "true"), ("max_qty", "0")]),
        "chk_pricing_plan_addon_rule_required_max_qty",
    )
    .await;
    // An **optional** add-on needs no bound at all, which is what keeps the
    // constraint a rule about `required` rather than a `NOT NULL` in disguise.
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_A, &[])).await;
    must_succeed(
        &conn,
        &insert_addon(ADDON_2, PLAN_A, &[("required", "true"), ("max_qty", "1")]),
    )
    .await;
}

/// An inverted quantity window admits nothing.
///
/// The row is not `required`, so `chk_…_required_max_qty` is satisfied by its
/// first disjunct and this constraint is what answers.
///
/// The NULL arms are exercised because they are what keeps a half-authored draft
/// savable: an author sets one bound in one request and the other in the next,
/// exactly as `chk_pricing_plan_purchase_qty` is written one table up.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_inverted_add_on_quantity_window_is_refused() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_be_rejected(
        &conn,
        &insert_addon(ADDON_1, PLAN_A, &[("min_qty", "5"), ("max_qty", "2")]),
        "chk_pricing_plan_addon_rule_qty_range",
    )
    .await;
    // Equal bounds are a window of exactly one quantity, which is a real rule.
    must_succeed(
        &conn,
        &insert_addon(ADDON_1, PLAN_A, &[("min_qty", "3"), ("max_qty", "3")]),
    )
    .await;
    // And either bound alone is open-ended.
    must_succeed(&conn, &insert_addon(ADDON_2, PLAN_A, &[("min_qty", "5")])).await;
    must_succeed(
        &conn,
        &insert_addon(
            "55555555-0000-0000-0000-000000000003",
            PLAN_A,
            &[("max_qty", "2")],
        ),
    )
    .await;
}

/// A step of zero admits either every quantity or none, and which one a selection
/// surface picks is undefined; a negative step names no selection at all.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_non_positive_add_on_step_is_refused() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    for step in ["0", "-1"] {
        must_be_rejected(
            &conn,
            &insert_addon(ADDON_1, PLAN_A, &[("step_qty", step)]),
            "chk_pricing_plan_addon_rule_step_qty",
        )
        .await;
    }
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_A, &[("step_qty", "1")])).await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_addon_rule` — the foreign key and the two trigger arms
// ---------------------------------------------------------------------------

/// The parent move, for the same reason as the phase table's: it is the only
/// statement the FK alone can refuse.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_add_on_rule_may_not_be_orphaned_by_renumbering_its_revision() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_A, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan SET revision = 5 \
             WHERE plan_id = '{PLAN_A}' AND revision = 0"
        ),
        "fk_pricing_plan_addon_rule_revision",
    )
    .await;
}

/// Arm 1's unshared statement: the DELETE.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_add_on_rule_of_a_frozen_revision_cannot_be_deleted() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_A, &[])).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_addon_rule WHERE addon_sku_id = '{ADDON_1}'"),
        "pricing_plan_addon_rule: DELETE of an add-on rule under a published plan revision is not permitted",
    )
    .await;

    // And under an `abandoned` parent, which is the state
    // `PlanRepo::abandon_draft` has to order around.
    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_addon(ADDON_1, PLAN_B, &[])).await;
    freeze(&conn, PLAN_B, "abandoned").await;
    must_be_rejected(
        &conn,
        &format!(
            "DELETE FROM bss.pricing_plan_addon_rule \
             WHERE addon_sku_id = '{ADDON_1}' AND plan_id = '{PLAN_B}'"
        ),
        "pricing_plan_addon_rule: DELETE of an add-on rule under a abandoned plan revision is not permitted",
    )
    .await;
}

/// Arm 2's unshared statements: the INSERT and the re-point.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_frozen_revision_acquires_no_add_on_rules() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &insert_addon(ADDON_1, PLAN_A, &[]),
        "pricing_plan_addon_rule: INSERT of an add-on rule under a published plan revision is not permitted",
    )
    .await;

    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_addon(ADDON_2, PLAN_B, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_addon_rule SET plan_id = '{PLAN_A}' \
             WHERE addon_sku_id = '{ADDON_2}' AND plan_revision = 0"
        ),
        "pricing_plan_addon_rule: UPDATE of an add-on rule under a published plan revision is not permitted",
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_descriptor_set`
// ---------------------------------------------------------------------------

fn insert_descriptor(plan: &str, overrides: &[(&str, &str)]) -> String {
    let base = [
        ("plan_id", format!("'{plan}'")),
        ("plan_revision", "0".to_owned()),
        ("tenant_id", format!("'{TENANT}'")),
    ]
    .into_iter()
    .map(|(column, value)| (column.to_owned(), value))
    .collect();
    render(
        "pricing_plan_descriptor_set",
        &with_overrides(base, overrides),
    )
}

/// Every column is nullable, and that is what makes `DESCRIPTOR_INCOMPLETE`
/// reachable at all.
///
/// `flow-plan-author` step 4 attaches descriptors **incrementally in `draft`**, so
/// a `NOT NULL` here would refuse the ordinary authoring path — and would make
/// `inst-ds-required` unreachable, because a column that cannot be missing is an
/// element that can never be named as missing. The row with no descriptor at all
/// is the state the pipeline exists to judge, so the schema has to hold it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_descriptor_set_stores_with_nothing_authored_yet() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_descriptor(PLAN_A, &[])).await;
    // And the fully authored one, including P5's extension object.
    seed_draft(&conn, PLAN_B).await;
    must_succeed(
        &conn,
        &insert_descriptor(
            PLAN_B,
            &[
                ("invoice_line_template", "'{planName} / {phase}'"),
                ("gl_code", "'4000-01'"),
                ("itemization_rule", "'per_line'"),
                ("additional_fields", "'{\"costCentre\":\"CC-7\"}'"),
            ],
        ),
    )
    .await;
    // A draft revision's copy is mutable and deletable.
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_descriptor_set SET gl_code = '4000-02' \
             WHERE plan_id = '{PLAN_B}' AND plan_revision = 0"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_descriptor_set WHERE plan_id = '{PLAN_B}'"),
    )
    .await;
}

/// A revision has **one** descriptor set, and the key is what says so.
///
/// This table is genuinely 1:1 where its two siblings are not, so the primary key
/// is the only object carrying that fact. A second set for one revision would
/// give Billing two disagreeing answers for the invoice line an ERP posts.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_revision_holds_exactly_one_descriptor_set() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_descriptor(PLAN_A, &[])).await;
    must_be_rejected(
        &conn,
        &insert_descriptor(PLAN_A, &[("gl_code", "'4000-09'")]),
        "pricing_plan_descriptor_set_pkey",
    )
    .await;
    // Another revision of the same plan has its own, which is the copy-forward.
    freeze(&conn, PLAN_A, "published").await;
    must_succeed(
        &conn,
        &format!(
            "INSERT INTO bss.pricing_plan \
             (plan_id, revision, tenant_id, lifecycle_state, created_by, created_at_utc) \
             VALUES ('{PLAN_A}', 1, '{TENANT}', 'draft', '{ACTOR}', '2026-08-03 09:00:00+00')"
        ),
    )
    .await;
    must_succeed(&conn, &insert_descriptor(PLAN_A, &[("plan_revision", "1")])).await;
}

/// The parent move, for the same reason as the two sibling tables'.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_descriptor_set_may_not_be_orphaned_by_renumbering_its_revision() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_descriptor(PLAN_A, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan SET revision = 5 \
             WHERE plan_id = '{PLAN_A}' AND revision = 0"
        ),
        "fk_pricing_plan_descriptor_set_revision",
    )
    .await;
}

/// Arm 1's unshared statement: the DELETE.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_descriptor_set_of_a_frozen_revision_cannot_be_deleted() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_descriptor(PLAN_A, &[])).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_descriptor_set WHERE plan_id = '{PLAN_A}'"),
        "pricing_plan_descriptor_set: DELETE of a descriptor set under a published plan revision is not permitted",
    )
    .await;

    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_descriptor(PLAN_B, &[])).await;
    freeze(&conn, PLAN_B, "abandoned").await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM bss.pricing_plan_descriptor_set WHERE plan_id = '{PLAN_B}'"),
        "pricing_plan_descriptor_set: DELETE of a descriptor set under a abandoned plan revision is not permitted",
    )
    .await;
}

/// Arm 2's unshared statements: the INSERT and the re-point.
///
/// The INSERT is the sharper of the two on a 1:1 table — it is how a revision
/// that published **without** a descriptor set (the publish having been refused
/// by `DESCRIPTOR_INCOMPLETE`, or an operator now wanting to "fix" a frozen one)
/// would acquire one afterwards.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_frozen_revision_acquires_no_descriptor_set() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    freeze(&conn, PLAN_A, "published").await;
    must_be_rejected(
        &conn,
        &insert_descriptor(PLAN_A, &[]),
        "pricing_plan_descriptor_set: INSERT of a descriptor set under a published plan revision is not permitted",
    )
    .await;

    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_descriptor(PLAN_B, &[])).await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_descriptor_set SET plan_id = '{PLAN_A}' \
             WHERE plan_id = '{PLAN_B}' AND plan_revision = 0"
        ),
        "pricing_plan_descriptor_set: UPDATE of a descriptor set under a published plan revision is not permitted",
    )
    .await;
}

// ---------------------------------------------------------------------------
// `pricing_plan_period_floor_cap` (D-319) — the five CHECKs and the trigger
// ---------------------------------------------------------------------------
//
// The `SQLite` mirror of every rule below is in
// `tests/sqlite_plan_period_floor_cap.rs`, and it is a *different set of
// objects*: three fixed-message `RAISE(ABORT, …)` triggers where Postgres has
// one PL/pgSQL function with two arms. What is shared is the five `CHECK`s,
// written identically on both engines — which is the whole argument for putting
// the market pair in a new table rather than in columns on `pricing_plan`, where
// `SQLite` could carry no `CHECK` at all (`pricing_price`).

fn insert_bound(plan: &str, currency: &str, region: &str, floor: &str, cap: &str) -> String {
    format!(
        "INSERT INTO bss.pricing_plan_period_floor_cap \
         (plan_id, plan_revision, currency, region, tenant_id, floor_minor, cap_minor) \
         VALUES ('{plan}', 0, '{currency}', '{region}', '{TENANT}', {floor}, {cap})"
    )
}

/// The world this table accepts, asserted before anything is refused.
///
/// Without it a table nothing could be written to at all would pass every
/// refusal below — the module doc's standing rule, applied to a fourth table.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_draft_revisions_bounds_are_insertable_mutable_and_deletable() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;

    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "us", "50000", "500000")).await;
    // One bound per market, any number of markets: the second differs only in
    // `region` and the third only in `currency`.
    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "ca", "40000", "NULL")).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "EUR", "us", "NULL", "45000")).await;
    must_succeed(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_period_floor_cap SET floor_minor = 60000 \
             WHERE plan_id = '{PLAN_A}' AND plan_revision = 0 AND currency = 'USD' \
             AND region = 'us'"
        ),
    )
    .await;
    must_succeed(
        &conn,
        &format!(
            "DELETE FROM bss.pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN_A}' AND plan_revision = 0"
        ),
    )
    .await;
}

/// Every amount `CHECK`, each pinned against the value one step away from it.
///
/// The controls are the point: `> 0` and `> 1000` both refuse a zero, and `<`
/// and `<=` both refuse an inverted pair — only one of each is the rule, and
/// only the accepted value tells them apart.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_bound_that_admits_no_bill_is_refused_by_its_own_check() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;

    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USD", "us", "0", "NULL"),
        "chk_pricing_plan_period_floor_cap_floor_positive",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USD", "us", "NULL", "0"),
        "chk_pricing_plan_period_floor_cap_cap_positive",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USD", "us", "50001", "50000"),
        "chk_pricing_plan_period_floor_cap_ordered",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USD", "us", "NULL", "NULL"),
        "chk_pricing_plan_period_floor_cap_present",
    )
    .await;

    // The controls, in the same order: one minor unit, one minor unit, an equal
    // pair (a fixed-fee plan is not a contradiction), and each one-sided shape —
    // the last of which is what proves `_ordered`'s explicit NULL arms are being
    // evaluated rather than silently satisfied by NULL propagation.
    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "us", "1", "NULL")).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "ca", "NULL", "1")).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "EUR", "de", "50000", "50000")).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "EUR", "fr", "50000", "NULL")).await;
}

/// The currency `CHECK`, pinned on both sides of its equality.
///
/// `length(currency) = 3` is an equality, so a case that only refuses the short
/// code would stay green under a `>` — and `length()` counts characters on both
/// engines, which is what makes this the same guard as the `SQLite` mirror
/// rather than a second one.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_currency_that_is_not_three_characters_is_refused_on_either_side() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;

    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "US", "us", "50000", "NULL"),
        "chk_pricing_plan_period_floor_cap_currency",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USDD", "us", "50000", "NULL"),
        "chk_pricing_plan_period_floor_cap_currency",
    )
    .await;

    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "us", "50000", "NULL")).await;
}

/// The key is the whole market, so one revision holds one bound per market.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn one_revision_holds_one_bound_per_market() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "us", "50000", "NULL")).await;

    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "USD", "us", "60000", "NULL"),
        "pricing_plan_period_floor_cap_pkey",
    )
    .await;
}

/// A bound cannot hang off a revision that does not exist.
///
/// **The append-only trigger answers, not the foreign key.** A `BEFORE ROW`
/// trigger runs ahead of constraint checking on this engine, and the trigger's
/// predicate ("a *draft* revision with this key exists") strictly implies the
/// key's — so on the insert path the key can never be reached second, exactly as
/// on the `SQLite` mirror. What is asserted about the key is therefore that it
/// is **declared over both columns**, which is the half a refusal could never
/// show: a single-column key would refuse this row identically and would let a
/// bound sit under revision 7 of a plan whose only revision is 0.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_bound_cannot_hang_off_a_revision_that_does_not_exist() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;

    // The whole sentence the trigger raises, not the fragment. `is not permitted`
    // ends the append-only refusal of every one of this suite's four tables - the
    // phase, the add-on rule, the descriptor set and this one - so a bound refused
    // by a neighbour's trigger satisfies the fragment, which is what the module doc
    // forbids ("assert the object, never the table") and what
    // `one_revision_holds_one_bound_per_market` above already does with
    // `pricing_plan_period_floor_cap_pkey`. Rendered through `bound_refusal` rather
    // than hand-typed, so a reworded trigger has one place to be visited.
    must_be_rejected(
        &conn,
        &insert_bound(PLAN_ABSENT, "USD", "us", "50000", "NULL"),
        &bound_refusal("INSERT", "missing"),
    )
    .await;

    let declared = conn
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT pg_get_constraintdef(oid) AS def FROM pg_constraint \
             WHERE conname = 'fk_pricing_plan_period_floor_cap_revision'"
                .to_owned(),
        ))
        .await
        .expect("query the constraint")
        .expect("the foreign key is declared");
    let def: String = declared.try_get("", "def").expect("read the definition");
    assert!(
        def.contains("(plan_id, plan_revision)") && def.contains("(plan_id, revision)"),
        "the composite FK must cover both key columns, got: {def}"
    );
}

/// Every verb is refused once the revision is frozen, INSERT included — the arm
/// that stops a minimum nobody approved being *added* to a published revision.
///
/// Each refusal is asserted against the trigger's **interpolated sentence**, not
/// against the table name. The name is carried by all four of this table's
/// CHECKs, by its primary key and by
/// `fk_pricing_plan_period_floor_cap_revision` too, so a bare-name fragment is
/// satisfied by any of them — the anti-pattern the module doc forbids — and the
/// append-only arm could be narrowed to the wrong verb, or to the wrong parent
/// state, with these three assertions still green.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_frozen_revisions_bounds_take_no_insert_no_update_and_no_delete() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_A).await;
    must_succeed(&conn, &insert_bound(PLAN_A, "USD", "us", "50000", "NULL")).await;
    freeze(&conn, PLAN_A, "published").await;

    must_be_rejected(
        &conn,
        &insert_bound(PLAN_A, "EUR", "de", "40000", "NULL"),
        &bound_refusal("INSERT", "published"),
    )
    .await;
    must_be_rejected(
        &conn,
        &format!(
            "UPDATE bss.pricing_plan_period_floor_cap SET floor_minor = 999999 \
             WHERE plan_id = '{PLAN_A}' AND plan_revision = 0"
        ),
        &bound_refusal("UPDATE", "published"),
    )
    .await;
    must_be_rejected(
        &conn,
        &format!(
            "DELETE FROM bss.pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN_A}' AND plan_revision = 0"
        ),
        &bound_refusal("DELETE", "published"),
    )
    .await;
}

/// The sentence `pricing_plan_period_floor_cap_append_only` raises, per verb and
/// per parent state (`pricing_plan_period_floor_cap`). Reproduced here rather than matched
/// loosely: the verb and the state are exactly the two halves a narrowed arm
/// would get wrong.
fn bound_refusal(verb: &str, parent_state: &str) -> String {
    format!(
        "pricing_plan_period_floor_cap: {verb} of a period bound under a {parent_state} \
         plan revision is not permitted"
    )
}

/// **`abandoned` is not `draft`** — which is what forces `abandon_draft` to drop
/// these rows before it flips the revision.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_abandoned_revisions_bounds_can_no_longer_be_dropped() {
    let conn = applied().await;
    seed_draft(&conn, PLAN_B).await;
    must_succeed(&conn, &insert_bound(PLAN_B, "USD", "us", "50000", "NULL")).await;
    freeze(&conn, PLAN_B, "abandoned").await;

    must_be_rejected(
        &conn,
        &format!(
            "DELETE FROM bss.pricing_plan_period_floor_cap \
             WHERE plan_id = '{PLAN_B}' AND plan_revision = 0"
        ),
        // The parent state is in the assertion: without it this case is
        // indistinguishable from the `published` one above.
        &bound_refusal("DELETE", "abandoned"),
    )
    .await;
}
