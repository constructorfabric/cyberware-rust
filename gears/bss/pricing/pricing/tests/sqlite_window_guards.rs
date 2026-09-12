//! The window store's guards on the executed `SQLite` mirror.
//!
//! `tests/postgres_window.rs` proves the canonical objects. This suite exists
//! because the mirror is not a copy of them: Postgres carries **one** PL/pgSQL
//! function with six `IF` arms, and `SQLite` — which has no procedural language
//! and whose `RAISE(ABORT, …)` takes a literal message only — carries **six
//! separate triggers**, each with a `WHEN` clause that has to re-state the early
//! return the Postgres arm above it gets for free. Those are six different
//! objects, and a mirror whose arms fire in the wrong order, or whose `WHEN`
//! clauses overlap, is a backend divergence no assertion about Postgres can see.
//!
//! The sixth is the act sequence, `trg_pricing_price_window_act_sequence`. It was
//! labelled nowhere and executed nowhere: the arms below said "five" and stopped
//! at `future_end`, so the one arm that names an act rather than bounding a value
//! was the one nothing here reached.
//!
//! Two of the differences are more than mechanical and each has a test here that
//! could not be written against Postgres at all:
//!
//! * **every instant is `text` here, so every comparison is lexicographic**, and
//!   the rule that makes that safe has two halves. Between two **stored** instants
//!   it is exact — not because the rendering is fixed-width, which it is not
//!   (`AutoSi` picks 0, 3, 6 or 9 fractional digits per value), but because the
//!   offset sign sorts below both the fraction separator and the digits, so a
//!   coarser rendering of an instant always precedes a finer one. That is review
//!   Z2-8's correction and
//!   [`instants_of_three_fractional_widths_inside_one_second_still_order`] executes
//!   it; on that ground `chk_pricing_price_window_interval` and the two ordering
//!   CHECKs compare the columns directly. Against **`CURRENT_TIMESTAMP`** it is not, and arm 5 is the
//!   only guard here with the clock on one side: `CURRENT_TIMESTAMP` renders
//!   `YYYY-MM-DD HH:MM:SS`, a **space** at byte 11 where the stored text has `T`,
//!   so the arm normalizes with `datetime(…)`.
//!
//!   **The fixtures below are therefore written in the rendering the store
//!   actually holds** — `2099-01-01T00:00:00+00:00`, measured off a `SeaORM`
//!   write, not a legible `2099-01-01 00:00:00 +00:00` — because a test using any
//!   other spelling proves nothing about the deployed comparison. That sentence
//!   used to be here inverted, and it was: the fixtures were space-separated
//!   throughout while arm 5 compared a `substr(…, 1, 19)` prefix that read `'T'`
//!   (0x54) against `' '` (0x20). Every stored instant on **today's UTC date**
//!   therefore sorted greater than the clock and read as future, the arm was
//!   inoperative for exactly that window, and a suite of cross-date fixtures —
//!   where the date prefix decides before byte 11 is reached — kept it green while
//!   an end that had elapsed an hour earlier could be moved thirty days into the
//!   future. [`the_mirror_refuses_an_end_that_passed_earlier_today`] is the case
//!   that would have caught it, and it computes its instants in SQL so that "past"
//!   and "today" are facts rather than fixtures that age.
//! * **the biconditional CHECKs compare booleans on a backend with no boolean
//!   type.** `(state IN ('active','expired')) = (activated_at IS NOT NULL)` is two
//!   `0`/`1` integers here, and it is worth executing rather than reasoning about.
//!
//! Raw SQL throughout, deliberately past every repository — the layer that cannot
//! see a guard stop refusing. `tests/sqlite_window_repo.rs` is the other half: what
//! the repository refuses, and the non-overlap rule no schema object carries.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use sea_orm::DatabaseConnection;

mod common;

use common::{exec, migrated_db, must_succeed};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const PLAN: &str = "22222222-2222-2222-2222-222222222222";
const PHASE: &str = "33333333-3333-3333-3333-333333333333";
const ACTOR: &str = "44444444-4444-4444-4444-444444444444";
const ROW: &str = "aaaaaaaa-0000-0000-0000-000000000001";
const OTHER_ROW: &str = "aaaaaaaa-0000-0000-0000-000000000002";
const WINDOW: &str = "bbbbbbbb-0000-0000-0000-000000000001";
const SECOND: &str = "bbbbbbbb-0000-0000-0000-000000000002";

/// Instants well clear of the clock in both directions, **in the rendering the
/// store actually holds** — RFC 3339 with a `T` separator and a `+00:00` offset,
/// which is what `SeaORM` writes into these `text` columns (measured). See the
/// module doc: a fixture in any other spelling would compare differently from a
/// real row against both `CURRENT_TIMESTAMP` and a sibling column.
const FUTURE_FROM: &str = "'2099-01-01T00:00:00+00:00'";
/// A future instant **inside** `[FUTURE_FROM, FUTURE_TO)`. It exists so that a
/// test moving `effective_from` leaves `effective_to > effective_from` and
/// `chk_pricing_price_window_interval` has nothing to say — otherwise the CHECK
/// answers and the trigger arm under test is shadowed by it.
const MID_FUTURE: &str = "'2099-03-01T00:00:00+00:00'";
const FUTURE_TO: &str = "'2099-06-01T00:00:00+00:00'";
const FURTHER_FUTURE: &str = "'2099-12-01T00:00:00+00:00'";
const PAST: &str = "'2020-01-01T00:00:00+00:00'";
const FURTHER_PAST: &str = "'2020-06-01T00:00:00+00:00'";
/// One second after [`FUTURE_FROM`], for the `activated_at` of a window whose
/// start is `FUTURE_FROM`: `chk_pricing_price_window_activation_order` requires an
/// activation not to precede the start it was the arrival of.
const ACTIVATED: &str = "'2099-01-01T00:00:01+00:00'";

/// **Today's UTC date, at 00:00:00, in the deployed rendering** — computed in SQL
/// so that it is a fact and not a fixture that ages.
///
/// Its whole purpose is to tie the *date* prefix with `CURRENT_TIMESTAMP` while
/// lying strictly in the past, which is the one shape arm 5's old `substr`
/// comparison read backwards. It is in the past at every instant of every day
/// except the exact zero of midnight, where it equals the clock and arm 5's `<=`
/// still fires — so the case holds around the clock.
const TODAY_MIDNIGHT: &str = "strftime('%Y-%m-%dT%H:%M:%S+00:00','now','start of day')";
/// Thirty days before [`TODAY_MIDNIGHT`], so an interval ending there is non-empty.
const A_MONTH_AGO: &str = "strftime('%Y-%m-%dT%H:%M:%S+00:00','now','start of day','-30 days')";

/// Reject, **and** for the stated reason.
///
/// The fragment is the whole assertion. Every CHECK name, every trigger name and
/// every literal message on this table contains `pricing_price_window`, so a test
/// that accepted any error naming the table would pass with the trigger it means
/// to prove switched off, refused instead by an arm it never intended to reach.
async fn must_be_rejected(conn: &DatabaseConnection, sql: &str, because: &str) {
    let err = exec(conn, sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the guard must reject: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("pricing_price_window"),
        "the rejection must name the guard it came from, got: {message}"
    );
    assert!(
        message.contains(because),
        "the rejection must be the one under test (`{because}`), got: {message}"
    );
}

/// A migrated mirror with the two parent price rows seeded.
async fn seeded() -> DatabaseConnection {
    let conn = migrated_db().await;
    for (id, charge_kind) in [(ROW, "recurring"), (OTHER_ROW, "one_time")] {
        must_succeed(
            &conn,
            &format!(
                "INSERT INTO pricing_price (
                     price_id, tenant_id, plan_id, currency, region, phase,
                     charge_kind, amount_minor, model_kind, lifecycle_state,
                     created_by, created_at_utc)
                 VALUES ('{id}', '{TENANT}', '{PLAN}', 'USD', 'EU', '{PHASE}',
                     '{charge_kind}', 1000, 'flat', 'published', '{ACTOR}',
                     '2026-08-04T09:00:00+00:00')"
            ),
        )
        .await;
    }
    conn
}

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
        ("created_at", "'2026-08-04T09:00:00+00:00'".to_owned()),
    ]
    .into_iter()
    .map(|(column, value)| (column.to_owned(), value))
    .collect()
}

/// A window row that is deliberately **out of the occupancy set**.
///
/// `excl_pricing_price_window_no_overlap` (D-352) put non-overlap in the schema, partial over
/// `('scheduled','active')`. Several cases in this file seed many windows on one
/// price row to exercise a *timestamp* property — the interval CHECK's
/// comparisons, and the collation that makes text instants sort chronologically
/// — and those rows overlap each other incidentally. They are not claiming an
/// interval, so they are seeded `cancelled` rather than `scheduled`, which takes
/// them out of the new constraint's predicate without weakening what they test.
///
/// `cancelled` needs `cancelled_at` by `chk_pricing_price_window_cancelled_at`,
/// so it is set here rather than left to each caller.
fn insert_non_occupying(id: &str, overrides: &[(&str, &str)]) -> String {
    let mut all = vec![
        ("state", "'cancelled'"),
        ("cancelled_at", "'2026-08-04T09:30:00+00:00'"),
    ];
    all.extend_from_slice(overrides);
    insert(id, &all)
}

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
    format!("INSERT INTO pricing_price_window ({names}) VALUES ({values})")
}

fn update(id: &str, set: &str) -> String {
    format!("UPDATE pricing_price_window SET {set} WHERE window_id = '{id}'")
}

/// **Variable-width fractions still sort chronologically** — review Z2-8.
///
/// `pricing_price_window`'s migration doc is this crate's standing licence to compare
/// these `text` instants directly, and until 2026-08-18 the licence rested on a
/// false premise: it said the stored rendering is *"fixed-width, zero-padded and
/// monotonic"*. It is not fixed-width. `sqlx-sqlite` encodes a `OffsetDateTime` as
/// `to_rfc3339_opts(SecondsFormat::AutoSi, false)`, and `AutoSi` picks 0, 3, 6 or
/// 9 fractional digits per value — so one column holds 25-, 29-, 32- and
/// 35-character renderings side by side, and this gear's own writers mix them:
/// `check_authored_instant` bounds an *authored* instant at a millisecond and
/// explicitly excludes storage bookkeeping, so `OffsetDateTime::now_utc()` reaches `created_at`
/// at nanosecond precision.
///
/// The conclusion survives on a different argument, and this is that argument
/// executed rather than reasoned about: the offset sign sorts **below** both the
/// fraction separator and the digits — `'+'` is 0x2B, `'.'` is 0x2E, `'0'`–`'9'`
/// are 0x30–0x39 — so a coarser rendering always sorts before a finer one of the
/// same instant, which is chronological in both directions.
///
/// Three widths of one second are stored, in the wrong order on purpose, and both
/// consumers of the licence are asked: `chk_pricing_price_window_interval`
/// (`effective_to > effective_from`) must admit each pair, and an `ORDER BY` over
/// the column must return them in chronological order. Written because
/// "fixed-width" is exactly the premise that would license a `substr(…, 1, 25)`
/// normalization, which would truncate a nanosecond rendering mid-fraction and
/// reintroduce the class this file's module doc records having already shipped
/// once.
#[tokio::test]
async fn instants_of_three_fractional_widths_inside_one_second_still_order() {
    /// The same second at `AutoSi`'s 0-, 3- and 9-digit widths, coarsest first —
    /// which is chronological, each being a prefix-instant of the next.
    const WIDTHS: [&str; 3] = [
        "'2099-01-01T00:00:00+00:00'",
        "'2099-01-01T00:00:00.500+00:00'",
        "'2099-01-01T00:00:00.500000001+00:00'",
    ];
    let conn = seeded().await;

    // Every ordered pair the interval CHECK could see, submitted finer-as-`to`.
    // A CHECK reading these as out of order would refuse one of them.
    for (index, pair) in WIDTHS.windows(2).enumerate() {
        let id = format!("cccccccc-0000-0000-0000-00000000000{index}");
        must_succeed(
            &conn,
            &insert_non_occupying(
                &id,
                &[("effective_from", pair[0]), ("effective_to", pair[1])],
            ),
        )
        .await;
    }
    // ...and the widest pair, so the 0-digit / 9-digit comparison is asked too.
    must_succeed(
        &conn,
        &insert_non_occupying(
            "cccccccc-0000-0000-0000-00000000000f",
            &[("effective_from", WIDTHS[0]), ("effective_to", WIDTHS[2])],
        ),
    )
    .await;

    // The `ORDER BY` half. Inserted in the wrong order so the answer is the
    // engine's collation and not the insertion sequence.
    for (index, instant) in [WIDTHS[2], WIDTHS[0], WIDTHS[1]].into_iter().enumerate() {
        let id = format!("dddddddd-0000-0000-0000-00000000000{index}");
        must_succeed(
            &conn,
            &insert_non_occupying(
                &id,
                &[
                    ("effective_from", instant),
                    ("effective_to", "'2099-12-01T00:00:00+00:00'"),
                ],
            ),
        )
        .await;
    }
    let ordered = common::scalar(
        &conn,
        "SELECT group_concat(effective_from, '|') AS v FROM (
             SELECT effective_from FROM pricing_price_window
              WHERE window_id LIKE 'dddddddd-%'
              ORDER BY effective_from ASC)",
    )
    .await;
    assert_eq!(
        ordered,
        "2099-01-01T00:00:00+00:00|2099-01-01T00:00:00.500+00:00|\
         2099-01-01T00:00:00.500000001+00:00",
        "the coarser rendering must sort first: `'+'` (0x2B) is below both `'.'` \
         (0x2E) and the digits (0x30-0x39), which is what makes the order \
         chronological without the width being fixed"
    );
}

/// The moves that are supposed to work, so that the six arms below are proved to
/// be a whitelist rather than a blanket ban. A table nothing can be written to at
/// all would otherwise pass every refusal test in this file.
#[tokio::test]
async fn the_whole_life_of_a_window_is_storable_on_the_mirror() {
    let conn = seeded().await;
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
        &update(WINDOW, &format!("effective_to = {FURTHER_FUTURE}")),
    )
    .await;
    must_succeed(&conn, &update(WINDOW, "effective_to = NULL")).await;
    // And a bound taken back on. It is here because the window has to be bounded
    // again before it can expire — `chk_pricing_price_window_open_ended` states
    // `inst-ws-expire`'s "an open-ended window never expires" physically, so the
    // sequence that used to stand here (open the end, then expire) was a window
    // ending at an instant it did not have.
    must_succeed(
        &conn,
        &update(WINDOW, &format!("effective_to = {FURTHER_FUTURE}")),
    )
    .await;
    must_succeed(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'expired', expired_at = {FURTHER_FUTURE}"),
        ),
    )
    .await;
    // And the other edge out of `scheduled`, on a second window.
    must_succeed(&conn, &insert(SECOND, &[])).await;
    must_succeed(
        &conn,
        &update(
            SECOND,
            "state = 'cancelled', cancelled_at = '2026-08-04T10:00:00+00:00'",
        ),
    )
    .await;
}

/// The eight CHECKs on the backend with **no boolean type**, where each
/// biconditional is a comparison of two `0`/`1` integers.
///
/// Every row below is [`base_window`] with **exactly one** thing moved, so each
/// refusal is a fact about the constraint it names rather than about whichever
/// neighbour answered first. The three ordering constraints are the fiddly ones and
/// each entry says which of its siblings it had to stay clear of.
#[tokio::test]
async fn the_mirrors_checks_refuse_what_the_canonical_ones_do() {
    let conn = seeded().await;
    for (overrides, because) in [
        (
            vec![("state", "'paused'")],
            "chk_pricing_price_window_state",
        ),
        (
            vec![("effective_to", FUTURE_FROM)],
            "chk_pricing_price_window_interval",
        ),
        (
            vec![("activated_at", ACTIVATED)],
            "chk_pricing_price_window_activated_at",
        ),
        (
            vec![("expired_at", FUTURE_TO)],
            "chk_pricing_price_window_expired_at",
        ),
        (
            vec![("cancelled_at", "'2026-08-04T10:00:00+00:00'")],
            "chk_pricing_price_window_cancelled_at",
        ),
        // `inst-ws-activate` WHEN `now >= effectiveFrom`: an activation stamped
        // before the start it was the arrival of. The row is `active` with an
        // `activated_at`, so the biconditional is satisfied and the *order* is the
        // only thing wrong — and this is the lexicographic comparison of two stored
        // instants, which is why both are in the deployed rendering.
        (
            vec![("state", "'active'"), ("activated_at", PAST)],
            "chk_pricing_price_window_activation_order",
        ),
        // `inst-ws-expire` WHEN `now >= effectiveTo`, one edge over. `activated_at`
        // is set at the start (so the activation biconditional and the activation
        // *order* are both satisfied) and `effective_to` is present (so the
        // open-ended constraint is silent); the expiry instant is the only fault.
        (
            vec![
                ("state", "'expired'"),
                ("activated_at", FUTURE_FROM),
                ("expired_at", FUTURE_FROM),
            ],
            "chk_pricing_price_window_expiry_order",
        ),
        // `inst-ws-expire`, verbatim: an open-ended window never expires. With
        // `effective_to` NULL the interval CHECK admits the row and
        // `expired_at >= effective_to` evaluates to NULL — which a CHECK admits —
        // so this constraint is the only one that can answer, which is exactly why
        // it is a third constraint and not a clause of the second.
        (
            vec![
                ("state", "'expired'"),
                ("effective_to", "NULL"),
                ("activated_at", FUTURE_FROM),
                ("expired_at", FUTURE_TO),
            ],
            "chk_pricing_price_window_open_ended",
        ),
    ] {
        must_be_rejected(&conn, &insert(WINDOW, &overrides), because).await;
    }
    // The other direction of the activation biconditional, which is the one the
    // one-way form would accept: an `active` window with no `activated_at`, issued
    // as a sanctioned flip so that no trigger arm can answer first.
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, "state = 'active'"),
        "chk_pricing_price_window_activated_at",
    )
    .await;
}

/// The foreign key on the mirror, which had no test at all.
///
/// It is not a formality here: `PRAGMA foreign_keys` is **1** on this connection
/// (`sqlx` sets it), so the constraint is live and removing it from the migration
/// let a window name a price row that does not exist — with every read's key
/// resolution then having nothing to resolve. `tests/sqlite_window_repo.rs` cannot
/// prove it, because there the repository's scope gate answers first and by design
/// (`a_window_on_a_row_that_is_not_there_is_refused_before_the_foreign_key`), which
/// is the whole reason a raw statement is needed.
///
/// **This is the one refusal in this file [`must_be_rejected`] cannot assert**, and
/// the reason is the backend rather than a shortcut: `SQLite`'s foreign-key error is
/// the bare string `FOREIGN KEY constraint failed`, naming neither the constraint
/// nor the table, where Postgres names `fk_pricing_price_window_price` outright.
/// The fragment is still unique inside this suite — no CHECK name and no trigger
/// message here contains it — so the assertion stays as sharp as the driver allows.
#[tokio::test]
async fn the_mirror_refuses_a_window_on_a_price_row_that_does_not_exist() {
    let conn = seeded().await;
    let sql = insert(
        WINDOW,
        &[("price_id", "'aaaaaaaa-0000-0000-0000-0000000000ff'")],
    );
    let err = exec(&conn, &sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("the foreign key must reject: {sql}"));
    let message = err.to_string();
    assert!(
        message.contains("FOREIGN KEY"),
        "the rejection must be the foreign key's rather than a CHECK's or a \
         trigger's, got: {message}"
    );
}

/// Mirror arm 1.
#[tokio::test]
async fn the_mirror_refuses_a_delete() {
    let conn = seeded().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &format!("DELETE FROM pricing_price_window WHERE window_id = '{WINDOW}'"),
        "cancel is a state, not a deletion",
    )
    .await;
}

/// Mirror arm 2, with the statement only it can refuse: a **future** end moving on
/// a terminal row. `expired -> active` would be refused by arm 4 as well.
#[tokio::test]
async fn the_mirror_refuses_every_mutation_of_a_terminal_window() {
    let conn = seeded().await;
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

/// Mirror arm 3, with the shadow-escaping statement: a frozen column moving inside
/// a sanctioned flip. On this backend the escape is doubly necessary — arms 3 and 4
/// are separate triggers, and if arm 3's `WHEN` were wrong it is arm 4 that would
/// answer, with a different message and a green test.
///
/// **The target is [`MID_FUTURE`] and not [`FURTHER_FUTURE`], which is the whole
/// point of that constant.** With `FURTHER_FUTURE` the new start lands *after*
/// `effective_to`, so `chk_pricing_price_window_interval` refuses the statement
/// too and removing arm 3 reddened this test only on the message — verbatim the
/// failure mode it was written to escape. `MID_FUTURE` sits inside the interval,
/// so every CHECK is satisfied and arm 3 is the only object left that can answer.
#[tokio::test]
async fn the_mirror_refuses_a_frozen_column_inside_a_sanctioned_flip() {
    let conn = seeded().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(
            WINDOW,
            &format!(
                "state = 'active', activated_at = {ACTIVATED}, \
                 effective_from = {MID_FUTURE}"
            ),
        ),
        "bound to its price row and its start",
    )
    .await;
    // And the binding §6 names in its own sentence, to a row that really exists so
    // that the foreign key is satisfied and only the whitelist can answer.
    //
    // Butted against `WINDOW`'s end rather than sharing its span. `WINDOW` still
    // holds `[FUTURE_FROM, FUTURE_TO)` -- the flip above is *rejected*, so its
    // start never moved -- and since D-352 the schema refuses two occupying
    // windows on one price row. `[)` makes `effective_to ==` the next window's
    // start adjacency rather than collision, so this is the placement that keeps
    // both rows on `ROW`, which this case needs: what it asserts is the refusal
    // of a move to `OTHER_ROW`.
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
    must_be_rejected(
        &conn,
        &update(SECOND, &format!("price_id = '{OTHER_ROW}'")),
        "bound to its price row and its start",
    )
    .await;
}

/// Arm 3 on **`effective_from` alone**, which is the column the migration reports
/// as its headline divergence from `inst-ws-immutable`'s literal scope — and which
/// had no unshadowed proof at all.
///
/// The statement is the plainest one there is: a `scheduled` window's start moved
/// to another future instant *inside* its own interval. Arm 2 is silent (the row is
/// not terminal), arm 4 is silent (`state` does not move), arm 5 is silent
/// (`effective_to` does not move), and every CHECK is satisfied — the interval is
/// still non-empty and there is no `activated_at` for the ordering constraint to
/// have an opinion about. Arm 3 is the only object in the schema that refuses it,
/// and if it did not the freeze the migration calls "stricter than the instruction"
/// would not exist at all.
#[tokio::test]
async fn the_mirror_refuses_a_start_moved_to_another_future_instant() {
    let conn = seeded().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("effective_from = {MID_FUTURE}")),
        "bound to its price row and its start",
    )
    .await;
}

/// Mirror arm 4: `scheduled -> expired`, with `expired_at` set so that the CHECK
/// has nothing to say and the edge itself is the only thing wrong.
#[tokio::test]
async fn the_mirror_refuses_an_unsanctioned_transition() {
    let conn = seeded().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;
    must_be_rejected(
        &conn,
        &update(
            WINDOW,
            &format!("state = 'expired', activated_at = {ACTIVATED}, expired_at = {FUTURE_TO}"),
        ),
        "not a sanctioned one",
    )
    .await;
}

/// **Mirror arm 5, and the test this suite exists for.**
///
/// The comparison is against the clock rather than against a sibling column, so
/// both directions have to be executed: an end moved **into** the past is refused,
/// an end that has **already** passed cannot be moved even forward, and — the half
/// that would otherwise be silently broken — a legal move to a future instant still
/// lands. `the_whole_life_of_a_window_is_storable_on_the_mirror` carries that third
/// case, so a comparison that read every instant as past would redden there rather
/// than here.
///
/// **Both windows here start in the past on purpose.** With a `2099` start, moving
/// the end to `2020` leaves `effective_to < effective_from` and
/// `chk_pricing_price_window_interval` refuses the statement as well — so the first
/// assertion was shadowed and reddened on the message when arm 5 was removed. A
/// past start makes the target instant still later than the start, which leaves the
/// clock as the only thing wrong with it.
#[tokio::test]
async fn the_mirror_refuses_an_end_moved_into_or_out_of_the_past() {
    let conn = seeded().await;
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

    // An end that has already passed, seeded straight into that shape because no
    // sanctioned mutation can produce it — which is also the statement that makes a
    // born-in-a-state INSERT guard unavailable to this table.
    must_succeed(
        &conn,
        &insert(
            SECOND,
            &[
                // On `OTHER_ROW`, not `ROW`. Both windows in this case are
                // `active` and both claim an interval, and since D-352 the
                // schema refuses two occupying windows on one price row -- which
                // is not what this case is about. The guard under test is
                // per-window, so a second price row isolates it without
                // weakening it.
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

/// **Arm 5 on the one shape a text prefix reads backwards**, and the case that
/// would have caught the arm being switched off.
///
/// An end that elapsed earlier **today**, moved thirty days into the future. Every
/// other assertion in this file straddles a date boundary, where the date prefix
/// decides the comparison before byte 11 is reached — and byte 11 is where the
/// stored `T` met `CURRENT_TIMESTAMP`'s space. On a same-UTC-day instant the prefix
/// ties, `'T'` (0x54) beat `' '` (0x20), and arm 5 read the elapsed end as future
/// and said nothing. Measured against the deployed table before the fix: this
/// `UPDATE` was **accepted**, and through the repository an end an hour behind the
/// clock moved thirty days ahead.
///
/// Both instants are computed in SQL rather than written down, because "today" is
/// the one thing a literal cannot be. Arms 2, 3 and 4 are silent — the row is
/// `active`, no frozen column moves, `state` does not move — and every CHECK is
/// satisfied, so arm 5 is the only object that can answer.
#[tokio::test]
async fn the_mirror_refuses_an_end_that_passed_earlier_today() {
    let conn = seeded().await;
    must_succeed(
        &conn,
        &insert(
            WINDOW,
            &[
                ("effective_from", A_MONTH_AGO),
                ("effective_to", TODAY_MIDNIGHT),
                ("state", "'active'"),
                ("activated_at", A_MONTH_AGO),
            ],
        ),
    )
    .await;
    must_be_rejected(
        &conn,
        &update(WINDOW, &format!("effective_to = {FURTHER_FUTURE}")),
        "only be moved while it is in the future",
    )
    .await;
}

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
async fn a_negative_mutation_seq_is_refused() {
    let conn = seeded().await;
    must_be_rejected(
        &conn,
        &insert(WINDOW, &[("mutation_seq", "-1")]),
        "chk_pricing_price_window_mutation_seq",
    )
    .await;
    must_succeed(&conn, &insert(WINDOW, &[("mutation_seq", "0")])).await;
}

/// **Mirror arm 6, the act sequence**, which had no executed refusal on either
/// engine before this case and its Postgres twin
/// [`postgres_window::the_act_sequence_moves_by_one_act_at_a_time`].
///
/// It was pinned three ways and executed none: the trigger-name roster, the
/// trigger-body digest roster and both schema goldens all carry it, and
/// `sqlite_append_only::the_window_frozen_whitelist_names_every_content_column_the_table_holds`
/// *exempts* `mutation_seq` from the frozen-column census on the grounds that this
/// arm guards it. Those pin the arm's **text**, which is the half an author
/// regenerates in the same edit that breaks it; nothing said it still refused.
///
/// `mutation_seq` is a window's `If-Match` tag, so a reused name is D-184's
/// approval loop with no exit: the unit was opened against act *n*, the act is
/// renumbered, and the retry after the approve names a subject no unit exists for.
///
/// Three statements and three different faults. `+2` skips a name, which a redrive
/// reads as a lost act; `-1` reuses one; `+1` is the only move the sequence admits,
/// and it is here because an arm written `NEW.mutation_seq <> OLD.mutation_seq` —
/// refusing every move — passes both refusals on its own. The row stays
/// `scheduled` and no other column moves, so arms 1 to 5 are all silent and this
/// is the only object on the mirror that can answer.
#[tokio::test]
async fn the_mirror_refuses_an_act_sequence_that_skips_or_repeats() {
    let conn = seeded().await;
    must_succeed(&conn, &insert(WINDOW, &[])).await;

    must_be_rejected(
        &conn,
        &update(WINDOW, "mutation_seq = 2"),
        "moves by one act at a time",
    )
    .await;
    // Seeded at `1` before the backwards move, because from `0` there is no lower
    // number the column holds and a negative would be a different refusal.
    must_succeed(&conn, &update(WINDOW, "mutation_seq = 1")).await;
    must_be_rejected(
        &conn,
        &update(WINDOW, "mutation_seq = 0"),
        "moves by one act at a time",
    )
    .await;
    must_succeed(&conn, &update(WINDOW, "mutation_seq = 2")).await;
}
