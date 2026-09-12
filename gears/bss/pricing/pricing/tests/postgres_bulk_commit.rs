//! Phase 2's per-row optimistic commit **on the engine that runs in production**
//! (`design/12-operator-efficiency.md` §3 `inst-bk-phase2`, §4 `inst-bi-commit`,
//! `inst-bk-lock`, `inst-bs-done`; D-141, D-290, D-291, D-294).
//!
//! `sqlite_bulk_commit` drives [`commit_batch`] against the mirror and, until
//! this file, was the only place it ran at all. Two of its properties are not
//! provable there:
//!
//! **The holder in a conflict.** [`commit_batch`] takes its locks on
//! `db.conn()` — an autocommit connection — because `take_locks` requires one:
//! Postgres aborts an enclosing transaction on a failed statement, so inside one
//! the read that names the holding run cannot happen and the refusal degrades
//! from `BulkRowLocked` to `BulkLocksHeld`. `SQLite` does not abort on a
//! failed statement, so its version of
//! [`a_row_another_run_holds_conflicts_the_whole_batch`] passes whether or not
//! that contract is honoured. Here it does not: a `commit_batch` that moved its
//! locking inside a transaction would take the degraded error down the
//! `BulkLocksHeld` arm, leave the run `committing` and return its failure, and
//! redden this suite while every `SQLite` suite stayed green. `postgres_bulk_repo`
//! pins the degradation directly.
//!
//! **A run-level failure that is not any row's.** `commit_rows` treats four
//! refusals as per-row conflicts and everything else as the run's, and the run's
//! must still land terminal with its locks released — `inst-bs-done` says the
//! lock is released "either way", and D-294 is the commit where a `?` on
//! `commit_rows` made that false. Reaching that arm needs a store refusal the
//! domain types do not screen out first; `chk_pricing_price_billing_timing` is
//! one, and this engine names it in the message.
//!
//! Every case asserts **both halves** of the receipt, for
//! `sqlite_bulk_commit`'s reason: a receipt naming only the failures would be
//! satisfied by a run that committed nothing, and one naming only the successes
//! by a run that silently dropped the rest.
//!
//! Run with:
//! `cargo test -p cf-gears-bss-pricing --test postgres_bulk_commit -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod pg_support;

use std::collections::BTreeMap;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::bulk::{BulkKind, BulkState};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::import::ImportRow;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::bulk::{BULK_ROW_CONFLICT, CommitReceipt, commit_batch};
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPriceDraft, PriceRepo, bulk_repo,
};
use time::OffsetDateTime;

use pg_support::Pg;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_51);
const ACTOR: Uuid = Uuid::from_u128(0xac_50);
const CORRELATION: Uuid = Uuid::from_u128(0xc0_51);

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x50_c6))
}
fn phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_90))
}
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 9, hour, 0, 0)
}
fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}
fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: ACTOR,
        recorded_at: at(10),
        correlation_id: CORRELATION,
    }
}

fn key(region: &str) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("the class pairs with the cohort")
}

fn content(amount: i64) -> PriceContent {
    content_timed(amount, Some("advance"))
}

/// The same content with the billing timing chosen by the caller.
///
/// `billing_timing` stays an unvalidated `String` in the domain on purpose —
/// `price_record`'s module doc: the rule is Slice 6's registered one and an enum
/// minted in Slice 2 would be a second registration. The store is therefore the
/// only thing that checks it, through `chk_pricing_price_billing_timing`, which
/// is what lets a case here reach a **storage** refusal without inventing a fault
/// no caller could produce.
fn content_timed(amount: i64, timing: Option<&str>) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: timing.map(str::to_owned),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn row(scope_key: ScopeKey, amount: i64, if_match: Option<RowVersion>) -> ImportRow {
    ImportRow {
        scope_key,
        content: content(amount),
        if_match,
    }
}

/// The run's own provider and a **second** one for the price repository.
///
/// Two pools, because `commit_batch` holds a connection from the first for the
/// run's statements while the repository opens a transaction from the second for
/// every row it writes, and `pg_support` caps a pool at two connections. Sharing
/// one would make the suite's connection budget part of what it tests.
struct Harness {
    db: DBProvider<DbError>,
    prices: PriceRepo,
}

async fn harness() -> Harness {
    let pg = Pg::applied().await;
    let db = DBProvider::<DbError>::new(pg.db().await);
    let prices = PriceRepo::new(DBProvider::<DbError>::new(pg.db().await));
    Harness { db, prices }
}

/// Author a draft row and answer `(price_id, its version)`.
async fn seed_draft(h: &Harness, scope_key: ScopeKey, amount: i64) -> (Uuid, RowVersion) {
    let price_id = Uuid::now_v7();
    let record = h
        .prices
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key,
                content: content(amount),
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: CORRELATION,
            },
        )
        .await
        .expect("author the draft");
    (price_id, record.row_version)
}

async fn open_run(h: &Harness, client_key: &str) -> Uuid {
    let conn = h.db.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &scope(),
        NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: TENANT,
            kind: BulkKind::Import,
            client_key: client_key.to_owned(),
            request_hash: IdempotencyGate::payload_hash(client_key),
            report: serde_json::json!({}),
            submitted_by: ACTOR,
            submitted_at: at(10),
        },
    )
    .await
    .expect("open the run")
    .operation_id
}

/// Phase 2, asserting that the **run** did not fail.
async fn run_phase_2(h: &Harness, operation_id: Uuid, rows: &[ImportRow]) -> CommitReceipt {
    commit_batch(
        &h.db,
        &h.prices,
        &scope(),
        TENANT,
        operation_id,
        rows,
        stamp(),
        at(11),
    )
    .await
    .expect("the run itself does not fail")
}

async fn state_of(h: &Harness, operation_id: Uuid) -> BulkState {
    let conn = h.db.conn().expect("conn");
    bulk_repo::read(&conn, &scope(), TENANT, operation_id)
        .await
        .expect("read")
        .expect("exists")
        .state
}

async fn holder_of(h: &Harness, price_id: Uuid) -> Option<Uuid> {
    let conn = h.db.conn().expect("conn");
    bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
        .await
        .expect("read the lock")
}

/// A neighbouring run in `committing` holding `price_id`.
async fn a_neighbour_holding(h: &Harness, client_key: &str, price_id: Uuid) -> Uuid {
    let neighbour = open_run(h, client_key).await;
    let conn = h.db.conn().expect("conn");
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        neighbour,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("the neighbour enters committing");
    bulk_repo::take_locks(&conn, &scope(), TENANT, neighbour, &[price_id], at(11))
        .await
        .expect("and takes the row");
    neighbour
}

fn committed_rows(receipt: &CommitReceipt) -> Vec<usize> {
    receipt.committed.iter().map(|row| row.row).collect()
}

fn conflicted_rows(receipt: &CommitReceipt) -> Vec<usize> {
    receipt.conflicted.iter().map(|row| row.row).collect()
}

/// The happy path: two new keys are authored and the run reaches `completed`.
#[tokio::test]
#[ignore = "requires Docker"]
async fn new_keys_are_authored_and_the_run_completes() {
    let h = harness().await;
    let run = open_run(&h, "c-1").await;

    let receipt = run_phase_2(
        &h,
        run,
        &[row(key("eu"), 9_900, None), row(key("us"), 12_500, None)],
    )
    .await;

    assert_eq!(committed_rows(&receipt), vec![0, 1]);
    assert_eq!(conflicted_rows(&receipt), Vec::<usize>::new());
    assert_eq!(state_of(&h, run).await, BulkState::Completed);
    assert_ne!(
        receipt.committed[0].price_id, receipt.committed[1].price_id,
        "two keys are two rows"
    );
}

/// An edit lands under its `ETag` and moves the row that already held the key.
#[tokio::test]
#[ignore = "requires Docker"]
async fn an_edit_lands_under_its_etag_and_moves_the_row() {
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-2").await;

    let receipt = run_phase_2(&h, run, &[row(key("eu"), 12_500, Some(version))]).await;

    assert_eq!(committed_rows(&receipt), vec![0]);
    assert_eq!(
        receipt.committed[0].price_id, price_id,
        "an edit writes the row that already held the key, not a new one"
    );
    assert_eq!(state_of(&h, run).await, BulkState::Completed);
    assert_eq!(
        holder_of(&h, price_id).await,
        None,
        "and the run released what it held"
    );
}

/// **`inst-bk-phase2`'s whole content.** A stale `ETag` fails only its own row;
/// the rest are committed and stay committed, and the run ends
/// `completed_with_conflicts`, which is a success.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_stale_etag_fails_only_its_own_row_and_the_rest_stand() {
    let h = harness().await;
    let (_, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-3").await;

    let receipt = run_phase_2(
        &h,
        run,
        &[
            row(key("eu"), 12_500, Some(RowVersion::new(version.get() + 7))),
            row(key("us"), 7_500, None),
        ],
    )
    .await;

    assert_eq!(
        conflicted_rows(&receipt),
        vec![0],
        "the stale row, and only it"
    );
    assert_eq!(
        receipt.conflicted[0].violations[0].code, BULK_ROW_CONFLICT,
        "section 5's per-row code"
    );
    assert_eq!(
        committed_rows(&receipt),
        vec![1],
        "and its neighbour stands: committed rows stand"
    );
    assert_eq!(state_of(&h, run).await, BulkState::CompletedWithConflicts);
}

/// **The case the `SQLite` mirror cannot arm.**
///
/// A row another run holds conflicts the whole batch, nothing is committed, and
/// the report **names the run holding it** — `fr-concurrent-edit`'s requirement.
/// That last clause is the one this engine proves: naming the holder depends on
/// `take_locks` running on the autocommit connection `commit_batch` hands it,
/// because the read that resolves the holder happens after the colliding insert
/// and Postgres aborts a transaction on a failed statement.
///
/// So a `commit_batch` that took its locks inside a transaction would get
/// `RepoError::BulkLocksHeld` instead of `RepoError::BulkRowLocked`, fall down that
/// arm rather than the `BulkRowLocked` one, and return an error from
/// the whole run — reddening `run_phase_2`'s own expectation here, while the
/// mirror's twin of this case stayed green because `SQLite` does not abort on a
/// failed statement. `postgres_bulk_repo::take_locks_inside_a_transaction_loses_the_holder`
/// pins that degradation directly.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_row_another_run_holds_conflicts_the_whole_batch() {
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let (_, other_version) = seed_draft(&h, key("us"), 5_000).await;
    let neighbour = a_neighbour_holding(&h, "c-7", price_id).await;

    let run = open_run(&h, "c-8").await;
    let receipt = run_phase_2(
        &h,
        run,
        &[
            row(key("eu"), 12_500, Some(version)),
            row(key("us"), 7_500, Some(other_version)),
        ],
    )
    .await;

    assert_eq!(conflicted_rows(&receipt), vec![0, 1], "every row");
    assert_eq!(committed_rows(&receipt), Vec::<usize>::new());
    assert_eq!(state_of(&h, run).await, BulkState::CompletedWithConflicts);
    assert!(
        receipt.conflicted[0].violations[0]
            .detail
            .contains(&neighbour.to_string()),
        "the refusal must name the run holding the row, which is what \
         fr-concurrent-edit asks for and what an autocommit runner is what makes \
         possible: {}",
        receipt.conflicted[0].violations[0].detail
    );
}

/// **All or none, with the contended row second** (D-294).
///
/// Put it first and the very first insert collides, no partial set is ever
/// created, and the release under test is a no-op indistinguishable from a
/// release that works — the mis-arrangement a recent fix wave had to correct.
/// Here the run takes `free_row`, collides on `held_row`, and `free_row` must be
/// free again: a lock left behind belongs to an operation that is over and
/// freezes an interactive editor on that row.
///
/// That `free_row` is free afterwards does not by itself distinguish a release
/// that ran from a lock that was never taken — the end state is the same. The
/// direct observation lives one layer down, in
/// `postgres_bulk_repo::a_run_that_cannot_take_every_lock_releases_the_ones_it_took`,
/// which watches a lock it has already asserted held disappear across the failing
/// call. It cannot be staged here: this run reaches `committing` only through
/// `commit_batch` itself, so there is no point at which the test could take a
/// lock for it first.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_run_that_cannot_take_every_lock_releases_the_ones_it_took() {
    let h = harness().await;
    let (free_row, free_version) = seed_draft(&h, key("us"), 5_000).await;
    let (held_row, held_version) = seed_draft(&h, key("eu"), 9_900).await;
    let neighbour = a_neighbour_holding(&h, "c-12", held_row).await;

    let run = open_run(&h, "c-13").await;
    let receipt = run_phase_2(
        &h,
        run,
        &[
            row(key("us"), 7_500, Some(free_version)),
            row(key("eu"), 12_500, Some(held_version)),
        ],
    )
    .await;

    assert_eq!(conflicted_rows(&receipt), vec![0, 1]);
    assert_eq!(committed_rows(&receipt), Vec::<usize>::new());
    assert_eq!(
        holder_of(&h, free_row).await,
        None,
        "the row this run did take must be free again"
    );
    assert_eq!(
        holder_of(&h, held_row).await,
        Some(neighbour),
        "and the release took only this run's locks"
    );

    // And nothing was written: **each** seeded amount stands, by row.
    //
    // This used to be the single bound `amount_minor < 9_901` over every draft,
    // which the batch's own 7_500 write to the *free* row satisfies — and a
    // committed partial write to the free row is exactly the all-or-none failure
    // this case is named for. Only the second row's 12_500 was ever detected. The
    // map is compared whole, so a third row appearing is caught too.
    assert_eq!(
        stored_amounts(&h).await,
        BTreeMap::from([(free_row, 5_000), (held_row, 9_900)]),
        "no row moved: both seeded amounts stand"
    );
}

/// The plan's draft rows as `price_id -> amount_minor`.
async fn stored_amounts(h: &Harness) -> BTreeMap<Uuid, i64> {
    let conn = h.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &conn,
        &scope(),
        TENANT,
        plan(),
        &[LifecycleState::Draft],
    )
    .await
    .expect("read the drafts")
    .into_iter()
    .map(|record| {
        (
            record.price_id,
            record.row.amount_minor.expect("an amount").get(),
        )
    })
    .collect()
}

/// **A failure that is the run's and not any row's still lands the run terminal
/// and releases its locks** (D-294, `inst-bs-done`).
///
/// `commit_rows` reports four refusals per-row and returns everything else as the
/// run's failure. §4 draws no failure edge out of `committing`, so a run that
/// entered it and stopped would hold its rows against every interactive editor
/// with no operator remedy — D-37's lease takeover does not exist yet. The `?`
/// that once sat on `commit_rows` did exactly that; restoring it reddens the two
/// assertions below (the state stays `committing`, the lock stays held) and
/// nothing else.
///
/// The refusal is reached through `chk_pricing_price_billing_timing`, which the
/// domain deliberately does not screen — `billing_timing` is Slice 6's registered
/// rule and stays an unvalidated `String` here — so this is a fault a caller can
/// really produce rather than one manufactured for the case. Postgres names the
/// constraint, so the case can assert that the run failed **for this reason** and
/// not for some other storage fault.
#[tokio::test]
#[ignore = "requires Docker"]
async fn a_run_level_failure_still_lands_terminal_and_releases_its_locks() {
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-14").await;

    let failed = commit_batch(
        &h.db,
        &h.prices,
        &scope(),
        TENANT,
        run,
        &[ImportRow {
            scope_key: key("eu"),
            content: content_timed(12_500, Some("whenever")),
            if_match: Some(version),
        }],
        stamp(),
        at(11),
    )
    .await
    .expect_err("a storage refusal is the run's failure, not a row's conflict");
    assert!(
        format!("{failed:?}").contains("chk_pricing_price_billing_timing"),
        "the caller must still learn what failed: {failed:?}"
    );

    assert_eq!(
        state_of(&h, run).await,
        BulkState::CompletedWithConflicts,
        "the run reaches a terminal state even though it failed, because committing \
         has no failure edge to stop on"
    );
    assert_eq!(
        holder_of(&h, price_id).await,
        None,
        "and the lock is released either way, which is what inst-bs-done requires"
    );

    let conn = h.db.conn().expect("conn");
    let stored = bulk_repo::read(&conn, &scope(), TENANT, run)
        .await
        .expect("read")
        .expect("exists")
        .report;
    assert_eq!(
        stored["committed"].as_array().expect("an array").len(),
        0,
        "nothing committed: {stored}"
    );
    assert_eq!(
        stored["conflicted"].as_array().expect("an array").len(),
        1,
        "and the row is reported as un-attempted rather than lost: {stored}"
    );
}

/// `inst-bk-idem` replays the **stored** report to a retry, so what the run
/// stores is the contract — not merely what [`commit_batch`] returned.
///
/// On this engine the column is `jsonb` rather than the mirror's `text`, so the
/// round trip goes through a different mapping entirely.
#[tokio::test]
#[ignore = "requires Docker"]
async fn the_stored_report_carries_both_halves() {
    let h = harness().await;
    let (_, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-9").await;

    run_phase_2(
        &h,
        run,
        &[
            row(key("eu"), 12_500, Some(RowVersion::new(version.get() + 7))),
            row(key("us"), 7_500, None),
        ],
    )
    .await;

    let conn = h.db.conn().expect("conn");
    let stored = bulk_repo::read(&conn, &scope(), TENANT, run)
        .await
        .expect("read")
        .expect("exists")
        .report;
    assert_eq!(
        stored["committed"].as_array().expect("an array").len(),
        1,
        "the committed half survives the round trip: {stored}"
    );
    assert_eq!(
        stored["conflicted"][0]["violations"][0]["code"],
        serde_json::json!(BULK_ROW_CONFLICT),
        "and so does the conflicted half, code included: {stored}"
    );
}
