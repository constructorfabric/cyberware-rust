//! `pricing_price_window`, proved by **executing the statement each object must
//! refuse**, on Postgres.
//!
//! # What this suite is for
//!
//! `tests/postgres_migrations.rs` pins this table's CHECKs, its function and
//! its trigger **by name**, so none of them can vanish unnoticed. It issues no
//! DML, so it says the objects reached the server and nothing about what any of
//! them does — a CHECK replaced by `CHECK (1 = 1)` keeps that suite green. This is
//! the other half: one executed refusal per object, and each assertion names the
//! object the refusal came from.
//!
//! Every statement here is raw SQL, deliberately past every repository, because
//! the repository is exactly the layer that cannot see a guard stop refusing.
//!
//! # The three rules every test here follows
//!
//! **Execute the refusal.** A test that writes valid values is not evidence about
//! a guard.
//!
//! **Put the world in the state where the object under test is what answers.**
//! This table makes the hazard concrete twice over. The trigger has five arms and
//! several of them refuse many of the same statements: `expired -> active` is
//! refused by the terminal-history arm *and* by the transition arm, so a test
//! using it would prove nothing about either. Each test below therefore issues the
//! statement **only its own arm** can refuse, and each test's doc says which
//! shadow it was written to escape.
//!
//! **Assert the object, never the table.** Every CHECK and both triggers over this
//! table carry `pricing_price_window` in their name, as does the Postgres message
//! for a foreign-key violation. A test that accepted any error naming the table
//! would pass with the guard it means to prove switched off.
//!
//! # What this suite cannot prove, and where that proof lives
//!
//! **Non-overlap, and it is now half declarative.** Since D-352 the chain carries
//! `excl_pricing_price_window_no_overlap` — an `EXCLUDE USING gist` over
//! `(tenant_id, price_id, tstzrange(effective_from, effective_to, '[)'))` where the
//! state is `scheduled` or `active`, on the `btree_gist` extension
//! `m20260821_000002` installs — mirrored on `SQLite` as
//! `trg_pricing_price_window_no_overlap_insert`/`_update`. That covers two windows
//! on **one `price_id`**, and
//! [`a_second_overlapping_window_on_one_key_is_refused_by_the_store`] below
//! executes it.
//!
//! What stays procedural is the other half: one canonical scope key carried by
//! **two different `price_id`s**. The key is ten columns of `pricing_price` and
//! none of them is on the window row, so no index on this table can reach a
//! sibling row's interval; `window_repo::refuse_overlap` is where that half lives,
//! and `tests/sqlite_window_repo.rs::an_overlapping_window_is_refused` is its
//! proof.
//!
//! `idx_pricing_price_window_price` and `idx_pricing_price_window_due` are
//! **non-unique** indexes and refuse nothing; their only observable effect is on
//! plan choice, which is not a correctness property. Their presence is pinned by
//! name in `tests/postgres_migrations.rs` and that is the whole of what can be said
//! about them.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_window -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use bss_pricing::domain::instant::format_rfc3339;
use bss_pricing::domain::instant::utc_ymd_hms;
use pg_support::Pg;
use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use time::OffsetDateTime;

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const PLAN: &str = "22222222-2222-2222-2222-222222222222";
const PHASE: &str = "33333333-3333-3333-3333-333333333333";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";

/// The price row every window here hangs off.
const ROW: &str = "aaaaaaaa-0000-0000-0000-000000000001";
/// A second row, on a different `charge_kind` and therefore a different key. It is
/// here for the frozen-binding test: moving a window between rows needs a second
/// row to move it to.
const OTHER_ROW: &str = "aaaaaaaa-0000-0000-0000-000000000002";

const WINDOW: &str = "bbbbbbbb-0000-0000-0000-000000000001";
const SECOND: &str = "bbbbbbbb-0000-0000-0000-000000000002";

/// Instants well clear of the clock in both directions. The trigger's fifth arm
/// compares `effective_to` against `now()`, so "future" and "past" here have to be
/// facts rather than fixtures that age.
const FUTURE_FROM: &str = "'2099-01-01 00:00:00+00'";
/// A future instant **inside** `[FUTURE_FROM, FUTURE_TO)`.
///
/// It exists because of a shadow: with `FURTHER_FUTURE` as the target, a statement
/// moving `effective_from` leaves `effective_to < effective_from` and
/// `chk_pricing_price_window_interval` refuses it too, so removing the trigger arm
/// under test reddened only the message. An instant *inside* the interval satisfies
/// every CHECK and leaves the whitelist arm as the only object that can answer.
const MID_FUTURE: &str = "'2099-03-01 00:00:00+00'";
const FUTURE_TO: &str = "'2099-06-01 00:00:00+00'";
const FURTHER_FUTURE: &str = "'2099-12-01 00:00:00+00'";
const PAST: &str = "'2020-01-01 00:00:00+00'";
const FURTHER_PAST: &str = "'2020-06-01 00:00:00+00'";

/// The `activated_at` of a window whose start is [`FUTURE_FROM`].
///
/// **Not `now()`**, and the difference is a shadow rather than a style:
/// `chk_pricing_price_window_activation_order` requires an activation not to precede
/// the start it was the arrival of (`inst-ws-activate` WHEN `now >= effectiveFrom`),
/// so `activated_at = now()` on a row starting in 2099 is refused by that CHECK —
/// and every test below that used it to satisfy the *biconditional* would have been
/// answered by the ordering constraint instead. Every flip timestamp here is
/// therefore the boundary the flip was the arrival of.
const ACTIVATED: &str = "'2099-01-01 00:00:01+00'";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A fresh database carrying the applied chain, on the one shared server, with the
/// two parent price rows seeded.
///
/// **One** container for the whole binary and a `CREATE DATABASE` per test — the
/// idiom `tests/pg_support/mod.rs` establishes, and it is not a performance
/// preference: a container per test produced sporadic `PortNotExposed` panics, and
/// under guard-by-removal a spurious red is indistinguishable from the second test
/// a removal was not supposed to redden.
async fn applied() -> DatabaseConnection {
    applied_pg().await.1
}

/// The same, keeping the [`Pg`] handle so a racer can take **its own pool**.
///
/// `Pg::db()` per racer is the concurrency suite's rule and not an ergonomic
/// preference: two tasks sharing one pool can serialize on a connection rather than
/// on the object under test, and a concurrency suite must not have its concurrency
/// supplied by luck.
async fn applied_pg() -> (Pg, DatabaseConnection) {
    let pg = Pg::applied().await;
    let conn = pg.raw().await;
    for (id, charge_kind) in [(ROW, "recurring"), (OTHER_ROW, "one_time")] {
        must_succeed(
            &conn,
            &format!(
                "INSERT INTO bss.pricing_price (
                     price_id, tenant_id, plan_id, currency, region, phase,
                     charge_kind, amount_minor, model_kind, lifecycle_state,
                     created_by, created_at_utc)
                 VALUES ('{id}', '{TENANT}', '{PLAN}', 'USD', 'EU', '{PHASE}',
                     '{charge_kind}', 1000, 'flat', 'published', '{ACTOR}',
                     '2026-08-04 09:00:00+00')"
            ),
        )
        .await;
    }
    (pg, conn)
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
/// over this table names the table too — and because two of the trigger's five arms
/// can refuse the same statement, so a test that accepted any refusal would be
/// green about whichever arm happened to answer.
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

/// A window row's columns, as a name/value list a test may move one entry of.
fn base_window(id: &str) -> Vec<(String, String)> {
    [
        ("window_id", format!("'{id}'")),
        ("tenant_id", format!("'{TENANT}'")),
        ("price_id", format!("'{ROW}'")),
        ("effective_from", FUTURE_FROM.to_owned()),
        ("effective_to", FUTURE_TO.to_owned()),
        ("state", "'scheduled'".to_owned()),
        ("reason_code", "'priceIncrease'".to_owned()),
        ("created_by", format!("'{ACTOR}'")),
        ("created_at", "'2026-08-04 09:00:00+00'".to_owned()),
    ]
    .into_iter()
    .map(|(column, value)| (column.to_owned(), value))
    .collect()
}

/// `INSERT` of [`base_window`] with the named columns replaced or added.
///
/// Every refusal below is this row with **exactly one** thing moved, which is what
/// makes each of them a fact about the object it names rather than about whichever
/// neighbour answered first.
fn insert(id: &str, overrides: &[(&str, &str)]) -> String {
    let mut columns = base_window(id);
    for (name, value) in overrides {
        match columns.iter_mut().find(|(column, _)| column == name) {
            Some(slot) => (*value).clone_into(&mut slot.1),
            None => columns.push(((*name).to_owned(), (*value).to_owned())),
        }
    }
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
    format!("INSERT INTO bss.pricing_price_window ({names}) VALUES ({values})")
}

fn update(id: &str, set: &str) -> String {
    format!("UPDATE bss.pricing_price_window SET {set} WHERE window_id = '{id}'")
}

// ---------------------------------------------------------------------------
// The world: what the table accepts
// ---------------------------------------------------------------------------

/// The valid rows and the sanctioned flips, first. Without them every refusal
/// below would pass against a table that refuses everything, and every trigger arm
/// would be indistinguishable from an unconditional ban.
///
/// The whole life of one window, in the order it is lived: scheduled, active,
/// expired — each flip stamping its own column, and the expiry keeping the
/// `activated_at` it was given, which `chk_pricing_price_window_activated_at`
/// requires of it.
///
/// **Each flip is stamped at the boundary it was the arrival of**, because §4's
/// edges are conditional and `chk_pricing_price_window_activation_order` /
/// `chk_pricing_price_window_expiry_order` hold the durable half of those
/// conditions: an activation before the start, or an expiry before the end, is a
/// lifecycle nobody drove.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_window_lives_through_scheduled_active_and_expired() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_succeed(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'active', activated_at = {ACTIVATED}"),
        ),
    )
    .await;
    must_succeed(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'expired', expired_at = {FUTURE_TO}"),
        ),
    )
    .await;
}

/// The other edge out of `scheduled`, and the state that never carries an
/// `activated_at` because the only way into it leaves `scheduled`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_scheduled_window_cancels() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_succeed(
        &conn,
        &update(WINDOW, "state = 'cancelled', cancelled_at = now()"),
    )
    .await;
}

/// An open-ended window: `effective_to IS NULL` is a value (`inst-ws-expire`) and
/// `chk_pricing_price_window_interval` is written to admit it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_open_ended_window_is_storable() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[("effective_to", "NULL")])).await;
}

/// **§9's named false positive, at the storage layer.** Two windows sharing a
/// boundary instant — `effective_to = next.effective_from` — both land. This table
/// carries no cross-row rule at all, so what this pins is that nothing here refuses
/// adjacency; the interval CHECK's strict `>` is the reason it cannot.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn adjacent_windows_are_both_storable() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_succeed(
        &conn,
        &insert(
            SECOND,
            &[
                ("effective_from", FUTURE_TO),
                ("effective_to", FURTHER_FUTURE),
            ],
        ),
    )
    .await;
}

/// A **future** `effective_to` may be shortened, extended and opened — the whole
/// of the mutation `inst-ws-immutable` permits on an active window. Without this
/// the fifth trigger arm would be indistinguishable from one that freezes the end
/// outright.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_future_end_may_be_shortened_extended_and_opened() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_succeed(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'active', activated_at = {ACTIVATED}"),
        ),
    )
    .await;
    for target in [MID_FUTURE, FURTHER_FUTURE, "NULL"] {
        must_succeed(&conn, &update(WINDOW, &format!("effective_to = {target}"))).await;
    }
}

// ---------------------------------------------------------------------------
// The CHECK constraints, less `chk_pricing_price_window_reason_code`, which has no
// executed refusal here
// ---------------------------------------------------------------------------

/// The monotonic counter admits only what a caller can address.
///
/// `window_repo` reads it back through `u64::try_from` and answers `CorruptRow`
/// for anything else, so a poisoned row is a window no typed path can read and
/// no operator can repair through this gear. The sequence trigger is
/// `BEFORE UPDATE` and says nothing about an INSERT, so this is the one guard
/// standing between an out-of-band insert and a permanently unreadable window.
///
/// Zero is the admitted side and has to be asserted: every window is created at
/// it, so `> 0` would refuse every row this gear writes.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_negative_mutation_seq_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("mutation_seq", "-1")]),
        "chk_pricing_price_window_mutation_seq",
    )
    .await;
    must_succeed(&conn, &insert(WINDOW, &[("mutation_seq", "0")])).await;
}

/// The state enumeration. The row carries no flip timestamps, so all three
/// biconditional CHECKs are satisfied by an unknown state — `(false) = (false)` —
/// and this constraint is the only thing that can answer.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_state_outside_the_four_is_refused() {
    let conn = applied().await;
    for state in ["'paused'", "'superseded'", "'Scheduled'", "''"] {
        must_be_rejected(
            &conn,
            &insert(WINDOW, &[("state", state)]),
            "chk_pricing_price_window_state",
        )
        .await;
    }
}

/// `effective_to = effective_from` is not a zero-length window, it is a mistake —
/// and an inverted interval is the same mistake the other way round. Both are the
/// **only** thing wrong with the row, so the interval CHECK is what answers.
///
/// The strictness this proves is the same strictness that makes adjacency legal
/// between two rows: one comparison, two consequences.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_empty_or_inverted_interval_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("effective_to", FUTURE_FROM)]),
        "chk_pricing_price_window_interval",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert(
            WINDOW,
            &[("effective_from", FUTURE_TO), ("effective_to", FUTURE_FROM)],
        ),
        "chk_pricing_price_window_interval",
    )
    .await;
}

/// **The biconditional on `activated_at`, in both directions**, which is the
/// constraint this table's design note is mostly about.
///
/// The `active`-without-a-timestamp direction is issued as an **UPDATE of a
/// scheduled row**, not as an INSERT, and that is the world-state requirement:
/// `scheduled -> active` is a sanctioned transition and no frozen column moves, so
/// neither the transition arm nor the whitelist arm has anything to say and only
/// the CHECK can answer.
///
/// The other direction — a `scheduled` row claiming to have been activated — is
/// what the one-way form `state IN ('scheduled','cancelled') OR activated_at IS NOT
/// NULL` would **accept**, and it is a lie about when a price took effect.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_activation_timestamp_is_present_exactly_on_an_active_or_expired_window() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, "state = 'active'"),
        "chk_pricing_price_window_activated_at",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert(SECOND, &[("activated_at", ACTIVATED)]),
        "chk_pricing_price_window_activated_at",
    )
    .await;
    // And a `cancelled` window never carries one: the only edge into `cancelled`
    // leaves `scheduled`, so an `activated_at` there records an activation that
    // did not happen.
    must_be_rejected(
        &conn,
        &insert(
            SECOND,
            &[
                ("state", "'cancelled'"),
                ("cancelled_at", "now()"),
                ("activated_at", ACTIVATED),
            ],
        ),
        "chk_pricing_price_window_activated_at",
    )
    .await;
}

/// `chk_pricing_price_window_activation_order` — `inst-ws-activate`'s condition,
/// which the biconditional above says nothing about: an activation stamped
/// **before** the start it was the arrival of.
///
/// The world: the row is `active` and carries an `activated_at`, so the
/// biconditional is satisfied and the *order* is the only thing wrong. It is the
/// row `window_repo::transition` produced until 2026-08-04 when asked to activate a
/// window a week early — precisely the "activation that never happened" the
/// migration's INSERT-guard note names.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_activation_stamped_before_the_windows_start_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("state", "'active'"), ("activated_at", PAST)]),
        "chk_pricing_price_window_activation_order",
    )
    .await;
}

/// `chk_pricing_price_window_expiry_order` — `inst-ws-expire`'s condition, one edge
/// over.
///
/// `activated_at` sits at the start (so both the activation biconditional and the
/// activation *order* are satisfied) and `effective_to` is present (so the
/// open-ended constraint is silent), which leaves the expiry instant as the only
/// fault.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_expiry_stamped_before_the_windows_end_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(
            WINDOW,
            &[
                ("state", "'expired'"),
                ("activated_at", FUTURE_FROM),
                ("expired_at", FUTURE_FROM),
            ],
        ),
        "chk_pricing_price_window_expiry_order",
    )
    .await;
}

/// `chk_pricing_price_window_open_ended` — `inst-ws-expire` verbatim: an open-ended
/// window never expires.
///
/// **It is a third constraint rather than a clause of the second, and the world is
/// why.** With `effective_to` NULL the interval CHECK admits the row and
/// `expired_at >= effective_to` evaluates to NULL — which a CHECK admits — so
/// nothing else here can answer. Folding the two together would have made one
/// statement trip both and neither removal proof would have proved anything.
///
/// `window_repo::transition` would expire an open-ended window on request until
/// 2026-08-04, which is a key whose coverage the store says stopped at an instant
/// the row does not have.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_open_ended_window_that_claims_to_have_expired_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(
            WINDOW,
            &[
                ("state", "'expired'"),
                ("effective_to", "NULL"),
                ("activated_at", FUTURE_FROM),
                ("expired_at", FUTURE_TO),
            ],
        ),
        "chk_pricing_price_window_open_ended",
    )
    .await;
}

/// The expiry biconditional. The `expired`-without-a-timestamp row carries an
/// `activated_at` on purpose — without it `chk_pricing_price_window_activated_at`
/// answers first and this test would be green while saying nothing about the
/// constraint it names.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_expiry_timestamp_is_present_exactly_on_an_expired_window() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(
            WINDOW,
            &[("state", "'expired'"), ("activated_at", ACTIVATED)],
        ),
        "chk_pricing_price_window_expired_at",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("expired_at", FUTURE_TO)]),
        "chk_pricing_price_window_expired_at",
    )
    .await;
}

/// The cancellation biconditional. A `cancelled` row needs no `activated_at` — it
/// must not have one — so nothing else has anything to object to in either half.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_cancellation_timestamp_is_present_exactly_on_a_cancelled_window() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("state", "'cancelled'")]),
        "chk_pricing_price_window_cancelled_at",
    )
    .await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("cancelled_at", "now()")]),
        "chk_pricing_price_window_cancelled_at",
    )
    .await;
}

/// The foreign key: a window is bound to a price row that exists. Without it a
/// window could outlive — or precede — the row whose canonical scope key it is
/// filed under, and the key resolution every read performs would have nothing to
/// resolve.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_window_on_a_price_row_that_does_not_exist_is_refused() {
    let conn = applied().await;
    must_be_rejected(
        &conn,
        &insert(
            WINDOW,
            &[("price_id", "'aaaaaaaa-0000-0000-0000-0000000000ff'")],
        ),
        "fk_pricing_price_window_price",
    )
    .await;
}

// ---------------------------------------------------------------------------
// The trigger's six arms
// ---------------------------------------------------------------------------

/// **Arm 1.** "Cancel is a state, not a deletion" (§6, verbatim). No state, no
/// lifecycle and no actor may remove a window row, so the statement is issued
/// against a plain `scheduled` window — the most deletable thing the table has.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_window_row_cannot_be_deleted() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM bss.pricing_price_window WHERE window_id = '{WINDOW}'"),
        "cancel is a state, not a deletion",
    )
    .await;
}

/// **Arm 2**, and the statement only it can refuse.
///
/// An `expired` window with its `effective_to` moved to a **future** instant. Every
/// other arm is silent: no frozen column moves, `state` does not move, and the new
/// end is in the future so the fifth arm has nothing to say. The obvious statement
/// — `expired -> active` — is refused by the transition arm as well, so it would
/// have proved nothing about this one.
///
/// What it protects is not bookkeeping: an expired window's interval is what a
/// replay charges a past period at, and a store that let it move would let history
/// be repriced.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_terminal_window_admits_no_mutation_at_all() {
    let conn = applied().await;
    must_succeed(
        &conn,
        &insert(
            WINDOW,
            &[
                ("state", "'expired'"),
                ("activated_at", ACTIVATED),
                ("expired_at", FUTURE_TO),
            ],
        ),
    )
    .await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("effective_to = {FURTHER_FUTURE}")),
        "immutable history",
    )
    .await;
}

/// **Arm 3, and the arm that earns its keep**, for the `pricing_plan` finding: a
/// column whitelist is almost wholly shadowed by the transition arm beside it, so
/// an illegal statement usually gets refused *anyway* — by a different sentence,
/// and a test asserting on the message would redden without proving anything.
///
/// The statement only the whitelist can refuse is a frozen column moving **inside
/// a sanctioned flip**: `scheduled -> active` with `activated_at` set, which arms 2,
/// 4 and 5 all wave through, and `effective_from` moved along with it.
///
/// **And the target is [`MID_FUTURE`], because this test was itself shadowed.** With
/// `FURTHER_FUTURE` the new start landed after `effective_to`, so
/// `chk_pricing_price_window_interval` refused the statement as well and removing
/// arm 3 reddened this test on the message — the exact failure mode it exists to
/// escape, in the test the phase plan calls the one that earns its keep. Inside the
/// interval, every CHECK is satisfied and arm 3 is the only thing left.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_frozen_column_cannot_move_inside_a_sanctioned_flip() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'active', activated_at = {ACTIVATED}, effective_from = {MID_FUTURE}"),
        ),
        "bound to its price row and its start",
    )
    .await;
}

/// The same arm on **`effective_from` alone**, which is the column the migration
/// reports as its headline divergence from `inst-ws-immutable`'s literal scope — and
/// which had no unshadowed proof of its freeze at all until now.
///
/// `a_windows_price_binding_cannot_move` proves the arm, but only for `price_id`.
/// The statement here is the plainest one there is: a `scheduled` window's start
/// moved to another future instant *inside* its own interval. Arm 2 is silent (not
/// terminal), arm 4 is silent (`state` does not move), arm 5 is silent
/// (`effective_to` does not move), the interval stays non-empty and there is no
/// `activated_at` for the ordering CHECK to have an opinion about. Arm 3 is the only
/// object in the schema that refuses it — and if it did not, the freeze the
/// migration calls "stricter than the instruction" would not exist.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_windows_start_cannot_move_to_another_future_instant() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("effective_from = {MID_FUTURE}")),
        "bound to its price row and its start",
    )
    .await;
}

/// The same arm, on the binding §6 names in its own sentence: the window/price
/// binding is immutable after creation, and with it the canonical scope key the
/// window is filed under. The move is to a **real** second row, so the foreign key
/// is satisfied and the whitelist is the only thing that can refuse.
///
/// Without it a window could be walked from one key to another after the fact,
/// which every coverage and non-overlap answer already given about both keys would
/// silently stop being true of.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_windows_price_binding_cannot_move() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("price_id = '{OTHER_ROW}'")),
        "bound to its price row and its start",
    )
    .await;
}

/// **Arm 4.** `scheduled -> expired`: the row is not terminal, so arm 2 is silent;
/// no frozen column moves, so arm 3 is; `effective_to` does not move, so arm 5 is.
/// The edge itself is the only thing wrong.
///
/// `expired_at` is set in the same statement so that
/// `chk_pricing_price_window_expired_at` is satisfied — otherwise the CHECK would
/// answer and this test would be about the CHECK.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_unsanctioned_transition_is_refused() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'expired', activated_at = {ACTIVATED}, expired_at = {FUTURE_TO}"),
        ),
        "is not a sanctioned transition",
    )
    .await;
}

/// **Arm 5**, both halves, on an `active` window so that arm 2 stays silent.
///
/// A move **to** the past reprices an interval that has already elapsed; a move
/// **of** an end that has already elapsed resurrects coverage the key had lost.
/// §6's "permitted UPDATEs: … **future** `effective_to` adjustment" is one clause
/// and it forbids both.
///
/// **Both windows start in the past on purpose.** With a 2099 start, moving the end
/// to 2020 leaves `effective_to < effective_from` and
/// `chk_pricing_price_window_interval` refuses the statement too — so the first
/// assertion was shadowed and reddened on the message. A past start keeps the target
/// later than the start, which leaves the clock as the only thing wrong with it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_windows_end_cannot_be_moved_into_or_out_of_the_past() {
    let conn = applied().await;
    must_succeed(
        &conn,
        &insert(
            WINDOW,
            &[("effective_from", PAST), ("effective_to", FUTURE_TO)],
        ),
    )
    .await;
    must_succeed(
        &conn,
        &update(WINDOW, &format!("state = 'active', activated_at = {PAST}")),
    )
    .await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("effective_to = {FURTHER_PAST}")),
        "only be moved while it is in the future",
    )
    .await;

    // The other half: an end that has **already** passed may not be moved at all,
    // not even forward. The row is seeded straight into that shape, because no
    // sanctioned mutation can produce it — which is also the statement that makes a
    // born-in-a-state INSERT guard unavailable to this table.
    must_succeed(
        &conn,
        &insert(
            SECOND,
            &[
                // On `OTHER_ROW`, not `ROW`. Both windows in this case are
                // `active` and both claim an interval, and since D-352
                // (`pricing_price_window`) the schema refuses two occupying windows
                // on one price row -- which is not what this case is about. The
                // guard under test is per-window, so a second price row isolates
                // it without weakening it. The same repair
                // `sqlite_window_guards::the_mirror_refuses_an_end_moved_into_or_out_of_the_past`
                // took; this half of the pair was missed because that wave's
                // gates were the fast tier, which does not run this binary.
                //
                // Moving it here rather than nudging the interval matters: the
                // *statement under test* moves `effective_to` to
                // `FURTHER_FUTURE`, so a `SECOND` left on `ROW` would collide
                // with `WINDOW`'s `[PAST, FUTURE_TO)` whatever the seed looked
                // like, and the exclusion constraint would refuse the same
                // statement arm 5 is here to refuse.
                ("price_id", &format!("'{OTHER_ROW}'")),
                ("effective_from", PAST),
                ("effective_to", FURTHER_PAST),
                ("state", "'active'"),
                ("activated_at", PAST),
            ],
        ),
    )
    .await;
    must_be_rejected(
        &conn,
        &update(SECOND, &format!("effective_to = {FURTHER_FUTURE}")),
        "only be moved while it is in the future",
    )
    .await;
}

/// **Arm 6, the act sequence**, which had no executed refusal on either engine.
///
/// The arm's name was carried by the trigger-name roster, the trigger-body digest
/// roster and both schema goldens, and `sqlite_append_only`'s frozen-column
/// census *exempts* `mutation_seq` on the grounds that this arm guards it — so a
/// present, correct-looking and inoperative arm read as coverage on the one column
/// whose whole purpose is to name an act.
///
/// `mutation_seq` is what a window's `If-Match` tag is, and D-184's approval loop
/// is what a reused name costs: an operator approves a unit opened against act
/// *n*, the act is renumbered, and the retry that follows the approve names a
/// subject no unit was ever opened under.
///
/// Both refusals and the acceptance are needed and they are three different
/// faults. `+2` is a **skipped** name, which leaves a gap a redrive reads as a lost
/// act; `-1` is a **reused** name, which is the one that breaks the approval loop;
/// `+1` is the only move the sequence admits, and without it a guard reading
/// `NEW.mutation_seq <> OLD.mutation_seq` — refusing every move — would pass both
/// refusal halves. The row stays `scheduled` and no other column moves, so arms 1
/// to 5 are all silent and this arm is the only object in the schema that can
/// answer.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_act_sequence_moves_by_one_act_at_a_time() {
    let conn = applied().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;

    // A skipped name.
    must_be_rejected(
        &conn,
        &update(WINDOW, "mutation_seq = 2"),
        "moves by one act at a time",
    )
    .await;
    // A name run backwards. The row is seeded at `1` first, because from `0` there
    // is no lower number the column can hold and `-1` would be refused by the
    // signed-range conversion rather than by this arm.
    must_succeed(&conn, &update(WINDOW, "mutation_seq = 1")).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, "mutation_seq = 0"),
        "moves by one act at a time",
    )
    .await;
    // The accepting half, and the reason the two refusals do not prove the arm on
    // their own: an arm that refused *every* move would pass both of them.
    must_succeed(&conn, &update(WINDOW, "mutation_seq = 2")).await;
}

// ---------------------------------------------------------------------------
// The non-overlap rule's serialization point, now that it has one
// ---------------------------------------------------------------------------

/// **Two writers scheduling intersecting windows on one price row contend, and
/// exactly one of them commits.**
///
/// # What it pins, and what it used to pin
///
/// `window_repo::refuse_overlap` is a `SELECT` walk and its caller inserts
/// afterwards, with nothing in between: no lock, no advisory key and — until
/// `excl_pricing_price_window_no_overlap` — no constraint. Under `READ COMMITTED` two concurrent
/// mutations on one key therefore both read the key as free and both committed
/// one, and this case pinned *that* as an assertion rather than as a comment, so
/// the hole could not be lost. D-352 closed it in the schema and the case was
/// inverted rather than deleted, which is what its own doc had asked for.
///
/// So what it pins now is the positive race, in three parts: the two writers
/// **contend**, exactly one row survives, and the survivor is the first
/// committer's.
///
/// # The refusal's code is the load-bearing half
///
/// An `EXCLUDE` violation is SQLSTATE `23P01`, and `is_unique_violation` — the
/// only contention shape `contention_or_db` knows — matches `23505` alone. Without
/// `window_repo::overlap_or`'s recognizer the loser of this race would get "an
/// internal error occurred, retry later" in place of the named refusal the
/// uncontended path already gives it. A case that only counted surviving rows
/// would be green about a 500, so the error is asserted before the rows are.
///
/// # T1 is released by an **observed** lock wait, and that is not a style choice
///
/// A Postgres exclusion constraint does not refuse a conflicting inserter while
/// its rival is still open: it makes it *wait* on that transaction's id until the
/// rival commits or rolls back. So T2 cannot reach a verdict while T1 is parked,
/// and the shape this case first landed in — T1 parked until `release`, `release`
/// notified only after T2's verdict had been read — was a **harness deadlock**
/// with no database fault under it. Measured on the harness rather than reasoned
/// about, 2026-08-20: `pg_stat_activity` showed T1 `idle in transaction` and T2
/// `active` with `wait_event_type = Lock`, `wait_event = transactionid` and
/// `pg_blocking_pids = {T1}`, sitting there until T2's thirty-second timeout
/// fired. No timeout could have fixed it: T1's release was causally downstream of
/// the verdict it was blocking.
///
/// `pg_support::wait_until_a_backend_blocks` therefore drives the release, and it
/// is simultaneously the contention assertion — a run whose two writers never
/// contended panics there with "no backend ever blocked" instead of passing on a
/// coincidence.
///
/// # What it does not reach: the cross-mate half
///
/// The constraint is scoped by `price_id`, because a constraint sees only its own
/// table's columns and the canonical scope key is ten columns of `pricing_price`.
/// Both windows here are on **one** price row, which is the half the schema
/// guards. A `superseded` predecessor and its `published` successor share a key
/// and are two `price_id`s, and that pair is still guarded by `refuse_overlap`'s
/// read alone; `pricing_price_window`'s migration doc carries what closing it would cost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_second_overlapping_window_on_one_key_is_refused_by_the_store() {
    use std::sync::Arc;

    use bss_pricing::infra::storage::RepoError;
    use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};

    use tokio::sync::Notify;
    use toolkit_db::secure::AccessScope;
    use toolkit_db::secure::TxError;
    use uuid::Uuid;

    /// Generous but finite: a racer that never resolves is a refuted claim, not a
    /// slow one.
    const RACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    fn instant(month: u32) -> OffsetDateTime {
        utc_ymd_hms(2099, month, 1, 0, 0, 0)
    }

    fn stamp() -> bss_pricing::domain::audit::AuditStamp {
        bss_pricing::domain::audit::AuditStamp {
            actor_principal_id: Uuid::parse_str(ACTOR).expect("a uuid"),
            recorded_at: instant(1),
            correlation_id: Uuid::from_u128(0x_c0_11_a7),
        }
    }

    /// `[from, to)` on the seeded recurring row — so both windows land on **one**
    /// canonical scope key, which is what `refuse_overlap` is scoped by.
    fn window(id: u128, from: u32, to: u32) -> NewWindow {
        NewWindow {
            window_id: Uuid::from_u128(id),
            tenant_id: Uuid::parse_str(TENANT).expect("a uuid"),
            price_id: Uuid::parse_str(ROW).expect("a uuid"),
            effective_from: instant(from),
            effective_to: Some(instant(to)),
            reason_code: "raceProbe".to_owned(),
        }
    }

    let (pg, observer) = applied_pg().await;
    let tenant = Uuid::parse_str(TENANT).expect("a uuid");
    let scope = AccessScope::for_tenant(tenant);

    let inserted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // T1: `[2099-01, 2099-06)`. It writes and then **parks**, holding its
    // transaction open, so anything the second writer contends on would be held.
    let first = {
        let db = pg.db().await;
        let (inserted, release) = (Arc::clone(&inserted), Arc::clone(&release));
        let scope = scope.clone();
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        window_repo::schedule(txn, &scope, window(0x_e1, 1, 6), stamp()).await?;
                        inserted.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    inserted.notified().await;

    // T2: `[2099-03, 2099-12)` — it intersects T1's interval on the same key. The
    // `[t1, t3)` / `[t2, t4)` shape only the overlap rule can refuse, which is the
    // one `tests/sqlite_window_repo.rs::an_overlapping_window_is_refused` uses
    // against a single writer.
    //
    // **Spawned rather than awaited here**, because since `pricing_price_window` this
    // insert *blocks*: the exclusion constraint parks a conflicting inserter on
    // T1's transaction id rather than refusing it outright, so awaiting T2 before
    // releasing T1 is the deadlock this case's doc records.
    let second = {
        let db = pg.db().await;
        let scope = scope.clone();
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        window_repo::schedule(txn, &scope, window(0x_e2, 3, 12), stamp()).await?;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    // **They contend**, as a fact off the server rather than as a timing
    // observation: some backend of this test's own database is waiting on a lock
    // it has not been granted, which can only be T2 on T1. A run in which the two
    // never met panics here instead of passing.
    pg_support::wait_until_a_backend_blocks(&observer).await;

    // Only now does T1 commit, which is what turns T2's wait into a verdict.
    release.notify_one();
    tokio::time::timeout(RACE_TIMEOUT, first)
        .await
        .expect("T1 must finish once released")
        .expect("its task must not panic")
        .expect("T1 is uncontended and must commit");

    let second = tokio::time::timeout(RACE_TIMEOUT, second)
        .await
        .expect("T2 must reach a verdict once T1 releases")
        .expect("its task must not panic");

    // **T2 is refused, and the refusal is the domain's own.** Before
    // `excl_pricing_price_window_no_overlap` this `expect` was its mirror image - T2's insert
    // succeeded because it could not see T1's uncommitted row, and the assertion
    // below counted two committed windows on one key.
    //
    // The code matters as much as the refusal. An `EXCLUDE` violation is
    // SQLSTATE `23P01`, which `is_unique_violation` does not match, so without
    // `overlap_or`'s recognizer the loser of this race would get a 500. That is
    // what this assertion is really guarding.
    let refusal = second.expect_err("T2 must be refused: T1 committed the interval first");
    assert!(
        matches!(refusal, TxError::Domain(RepoError::WindowOverlap { .. })),
        "the constraint's refusal must arrive as the domain's own overlap error, \
         not as an unrecognized Db failure that renders 500: got {refusal:?}"
    );

    // The second half of the claim: the forbidden state is in the store. Read past
    // every repository, so a reader that filtered the overlap out could not hide it.
    let rows = observer
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT window_id::text AS id FROM bss.pricing_price_window \
                 WHERE price_id = '{ROW}' AND state = 'scheduled' \
                 AND effective_from < '2099-12-01 00:00:00+00' \
                 AND effective_to   > '2099-01-01 00:00:00+00' \
                 ORDER BY effective_from"
            ),
        ))
        .await
        .expect("read the window plane");
    assert_eq!(
        rows.len(),
        1,
        "exactly one window survives the race - the store now refuses the second \
         itself, where it used to hold both"
    );
    assert_eq!(
        rows.iter()
            .map(|r| r.try_get::<String>("", "id").expect("an id"))
            .collect::<Vec<_>>(),
        vec![Uuid::from_u128(0x_e1).to_string()],
        "and it is T1's - the transaction that committed first, not the one that \
         arrived second and found the interval taken"
    );
}

/// **The loser of a race on an *extension* is refused by name too, and until
/// 2026-08-21 it was not.**
///
/// # The sibling this case exists to stop being a sibling
///
/// [`a_second_overlapping_window_on_one_key_is_refused_by_the_store`] above proves
/// the recognizer on `window_repo::schedule`'s INSERT. Wiring
/// `window_repo::overlap_or` into that door **and into no other** leaves it short,
/// because
/// `pricing_price_window` has three writers — the INSERT, `transition`'s UPDATE and
/// `adjust_effective_to`'s. The constraint ranges over `effective_from`,
/// `effective_to` and `state`, so an UPDATE that moves an end re-offers the row's
/// interval to the exclusion index exactly as an INSERT offers a new one. Extending
/// an end is in fact the *stronger* claim of the two: it takes ground the row did
/// not previously hold.
///
/// So the loser of this race got `RepoError::Db("adjust pricing_price_window …")`
/// — 500, "an internal error occurred, please retry later" — where the identical
/// collision one door over answered `WINDOW_OVERLAP`. Review F1, 2026-08-21.
///
/// # Why this reaches the constraint and not `refuse_overlap`
///
/// `adjust_effective_to` is **not** unguarded: it walks the key with
/// `refuse_overlap` before it writes, and on an uncontended store that walk is what
/// answers. A case that simply extended a window into a committed neighbour would
/// therefore prove nothing about the mapping under test — it would be refused by
/// the read, one screen earlier, and stay green with the recognizer removed.
///
/// The interleaving is what puts the constraint in the answering seat, and every
/// step of it is load-bearing:
///
/// * the neighbour `[2099-04, 2099-06)` is written by T1 and **left uncommitted**,
///   so under `READ COMMITTED` T2's `refuse_overlap` cannot see it and passes for
///   real rather than being bypassed;
/// * T2's own interval `[2099-01, 2099-03)` does not touch it either, so nothing
///   in T2's pre-check has anything to say;
/// * T2's UPDATE then extends the end to `2099-05`, which does. Postgres parks T2
///   on T1's transaction id rather than refusing it outright — the same wait
///   [`a_second_overlapping_window_on_one_key_is_refused_by_the_store`]'s doc
///   records as a harness deadlock if the release is downstream of the verdict —
///   so `pg_support::wait_until_a_backend_blocks` drives the release and doubles as
///   the assertion that the two writers met at all.
///
/// # The assertion is the error the caller gets, not a row count
///
/// An `EXCLUDE` violation is SQLSTATE `23P01` and `is_unique_violation` matches
/// `23505` alone, so the untyped answer is `RepoError::Db` and the door renders it
/// `DomainError::Internal`. A case that counted surviving rows would be green about
/// that 500. The key and the rendered interval are asserted with the variant,
/// because they are what makes the refusal actionable: `WINDOW_OVERLAP` with an
/// empty key names no conflict an operator can go and resolve.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn an_extension_that_loses_a_race_is_refused_by_name_and_not_as_a_storage_failure() {
    use std::sync::Arc;

    use bss_pricing::infra::storage::RepoError;
    use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};

    use tokio::sync::Notify;
    use toolkit_db::secure::AccessScope;
    use toolkit_db::secure::TxError;
    use uuid::Uuid;

    /// Generous but finite: a racer that never resolves is a refuted claim, not a
    /// slow one.
    const RACE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// The window whose end is extended -- the subject of the race.
    const SUBJECT: u128 = 0x_f1;
    /// The window that takes the ground the extension wants, from the other
    /// transaction.
    const NEIGHBOUR: u128 = 0x_f2;

    fn instant(month: u32) -> OffsetDateTime {
        utc_ymd_hms(2099, month, 1, 0, 0, 0)
    }

    fn stamp() -> bss_pricing::domain::audit::AuditStamp {
        bss_pricing::domain::audit::AuditStamp {
            actor_principal_id: Uuid::parse_str(ACTOR).expect("a uuid"),
            recorded_at: instant(1),
            correlation_id: Uuid::from_u128(0x_c0_11_a7),
        }
    }

    fn window(id: u128, from: u32, to: u32) -> NewWindow {
        NewWindow {
            window_id: Uuid::from_u128(id),
            tenant_id: Uuid::parse_str(TENANT).expect("a uuid"),
            price_id: Uuid::parse_str(ROW).expect("a uuid"),
            effective_from: instant(from),
            effective_to: Some(instant(to)),
            reason_code: "raceProbe".to_owned(),
        }
    }

    let (pg, observer) = applied_pg().await;
    let tenant = Uuid::parse_str(TENANT).expect("a uuid");
    let scope = AccessScope::for_tenant(tenant);

    // The subject, committed before anybody races: `[2099-01, 2099-03)`. Born at
    // act zero, which is the `expected_seq` the extension below submits.
    {
        let db = pg.db().await;
        let scope = scope.clone();
        let (_db, seeded) = db
            .in_transaction::<(), RepoError, _>(move |txn| {
                Box::pin(async move {
                    window_repo::schedule(txn, &scope, window(SUBJECT, 1, 3), stamp()).await?;
                    Ok(())
                })
            })
            .await;
        seeded.expect("the subject window is uncontended and must be schedulable");
    }

    let inserted = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // T1: the neighbour at `[2099-04, 2099-06)`. It does not touch the subject's
    // `[2099-01, 2099-03)`, so T1 is uncontended on its way in and must commit; it
    // then **parks**, holding the row invisible to anyone reading under READ
    // COMMITTED.
    let neighbour = {
        let db = pg.db().await;
        let (inserted, release) = (Arc::clone(&inserted), Arc::clone(&release));
        let scope = scope.clone();
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        window_repo::schedule(txn, &scope, window(NEIGHBOUR, 4, 6), stamp())
                            .await?;
                        inserted.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    inserted.notified().await;

    // T2: extend the subject's end from `2099-03` to `2099-05`. Its `refuse_overlap`
    // walk runs against a store in which the neighbour does not exist yet, so the
    // pre-check passes on its merits -- this is the one door in the race whose read
    // guard is genuinely satisfied and whose *statement* is what collides.
    //
    // Spawned rather than awaited, for the reason the sibling case records: the
    // exclusion constraint parks a conflicting writer on T1's transaction id rather
    // than refusing it, so awaiting T2 before releasing T1 is a harness deadlock.
    let extension = {
        let db = pg.db().await;
        let scope = scope.clone();
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        window_repo::adjust_effective_to(
                            txn,
                            &scope,
                            tenant,
                            Uuid::from_u128(SUBJECT),
                            Some(instant(5)),
                            0,
                            stamp(),
                        )
                        .await?;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    // They contend, as a fact off the server: some backend of this database is
    // waiting on a lock it has not been granted, which can only be T2 on T1. A run
    // in which the extension never met the neighbour panics here rather than
    // passing on a coincidence.
    pg_support::wait_until_a_backend_blocks(&observer).await;

    release.notify_one();
    tokio::time::timeout(RACE_TIMEOUT, neighbour)
        .await
        .expect("T1 must finish once released")
        .expect("its task must not panic")
        .expect("T1 is uncontended and must commit");

    let extension = tokio::time::timeout(RACE_TIMEOUT, extension)
        .await
        .expect("T2 must reach a verdict once T1 releases")
        .expect("its task must not panic");

    let refusal = extension.expect_err("the extension must be refused: the neighbour committed it");
    let TxError::Domain(RepoError::WindowOverlap { key, requested, .. }) = &refusal else {
        panic!(
            "the constraint's refusal must arrive as the domain's own overlap error, \
             not as an unrecognized Db failure that renders 500: got {refusal:?}"
        );
    };
    assert!(
        key.contains(PLAN) && key.contains("recurring"),
        "the refusal must name the canonical scope key the collision happened on, \
         got: {key}"
    );
    assert_eq!(
        requested,
        &format!(
            "[{}, {})",
            format_rfc3339(instant(1)),
            format_rfc3339(instant(5))
        ),
        "and the interval the caller asked for, so the operator can see what it \
         would have taken"
    );

    // The store's half of the claim, read past every repository so that a reader
    // which filtered the state out could not hide it: both windows are there. The
    // refusal is of the *extension*, not of either row.
    let rows = observer
        .query_all_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT window_id::text AS id FROM bss.pricing_price_window \
                  WHERE price_id = '{ROW}' AND state = 'scheduled' \
                  ORDER BY effective_from"
            ),
        ))
        .await
        .expect("read the window plane");
    assert_eq!(
        rows.iter()
            .map(|r| r.try_get::<String>("", "id").expect("an id"))
            .collect::<Vec<_>>(),
        vec![
            Uuid::from_u128(SUBJECT).to_string(),
            Uuid::from_u128(NEIGHBOUR).to_string(),
        ],
        "the subject and the neighbour both stand: the loser of this race is a \
         statement, not a window"
    );

    // And the subject still ends where it did, because the whole transaction rolled
    // back. Compared in SQL against a literal, so no rendering of a timestamptz -
    // which is a session setting - can enter the assertion.
    let unmoved = observer
        .query_one_raw(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            format!(
                "SELECT count(*)::bigint AS n FROM bss.pricing_price_window \
                  WHERE window_id = '{}' \
                    AND effective_to = timestamptz '2099-03-01 00:00:00+00'",
                Uuid::from_u128(SUBJECT)
            ),
        ))
        .await
        .expect("read the subject's end")
        .expect("one row")
        .try_get::<i64>("", "n")
        .expect("read the count");
    assert_eq!(
        unmoved, 1,
        "the extension rolled back whole - the end it would have taken is not there"
    );
}
