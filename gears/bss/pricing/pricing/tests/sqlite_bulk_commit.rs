//! Phase 2's per-row optimistic commit, executed against a real store
//! (`design/12-operator-efficiency.md` §3 `inst-bk-phase2`, §4 `inst-bi-commit`;
//! D-141, D-291).
//!
//! **What this suite is about is the partial result.** Phase 2's product is
//! `{committed, conflicted}`: a conflict fails one row and the rest stand. So
//! every case here asserts *both* halves — which rows landed and which did not —
//! because a receipt that named only the failures would be satisfied by a run
//! that committed nothing, and one that named only the successes by a run that
//! silently dropped the rest.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::bulk::{BulkKind, BulkState};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::import::ImportRow;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::bulk::{
    ABORT_NOTE, ABORTED_MEMBER, BULK_ROW_CONFLICT, CommitReceipt, PRIOR_REPORT_MEMBER,
    abandon_committing_run, commit_batch,
};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::price;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPriceDraft, PriceRepo, bulk_repo,
};
use time::OffsetDateTime;

use sea_orm::{ColumnTrait, Condition, ConnectionTrait, DbErr, EntityTrait, Statement};
use sea_orm_migration::{MigrationName, MigrationTrait, MigratorTrait, SchemaManager};
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
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
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
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

struct Harness {
    provider: DBProvider<DbError>,
    prices: PriceRepo,
}

async fn harness() -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let prices = PriceRepo::new(provider.clone());
    Harness { provider, prices }
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
    let conn = h.provider.conn().expect("conn");
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

async fn run_phase_2(h: &Harness, operation_id: Uuid, rows: &[ImportRow]) -> CommitReceipt {
    commit_batch(
        &h.provider,
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
    let conn = h.provider.conn().expect("conn");
    bulk_repo::read(&conn, &scope(), TENANT, operation_id)
        .await
        .expect("read")
        .expect("exists")
        .state
}

fn committed_rows(receipt: &CommitReceipt) -> Vec<usize> {
    receipt.committed.iter().map(|row| row.row).collect()
}

fn conflicted_rows(receipt: &CommitReceipt) -> Vec<usize> {
    receipt.conflicted.iter().map(|row| row.row).collect()
}

#[tokio::test]
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

#[tokio::test]
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
}

#[tokio::test]
async fn a_stale_etag_fails_only_its_own_row_and_the_rest_stand() {
    // **`inst-bk-phase2`'s whole content.** The conflicted row is named for
    // retry; the others are committed and stay committed.
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
    assert_eq!(
        state_of(&h, run).await,
        BulkState::CompletedWithConflicts,
        "which is a success, not a failure"
    );
}

#[tokio::test]
async fn a_row_asserting_no_version_over_an_existing_draft_conflicts() {
    // Silent overwrite never happens in either direction: without a version there
    // is nothing to compare, so the row is refused rather than written over
    // somebody's edit.
    let h = harness().await;
    seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-4").await;

    let receipt = run_phase_2(&h, run, &[row(key("eu"), 12_500, None)]).await;

    assert_eq!(conflicted_rows(&receipt), vec![0]);
    assert!(
        receipt.conflicted[0].violations[0].detail.contains("ETag"),
        "the refusal says what to do: {}",
        receipt.conflicted[0].violations[0].detail
    );
    assert_eq!(committed_rows(&receipt), Vec::<usize>::new());
}

#[tokio::test]
async fn a_row_asserting_a_version_over_nothing_conflicts_too() {
    // The mirror fault: the row it meant to edit is gone. Reported rather than
    // silently turned into a create, which would resurrect a row an operator
    // deliberately abandoned.
    let h = harness().await;
    let run = open_run(&h, "c-5").await;

    let receipt = run_phase_2(&h, run, &[row(key("eu"), 12_500, Some(RowVersion::new(0)))]).await;

    assert_eq!(conflicted_rows(&receipt), vec![0]);
    assert_eq!(committed_rows(&receipt), Vec::<usize>::new());
    assert_eq!(state_of(&h, run).await, BulkState::CompletedWithConflicts);
}

#[tokio::test]
async fn the_locks_are_released_when_the_run_ends() {
    // `inst-bk-lock` holds rows only while the run is committing. A lock left
    // behind is an interactive editor refused forever by a run that finished.
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-6").await;

    run_phase_2(&h, run, &[row(key("eu"), 12_500, Some(version))]).await;

    let conn = h.provider.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
            .await
            .expect("read the lock"),
        None,
        "the run released what it held"
    );
}

#[tokio::test]
async fn a_row_another_run_holds_conflicts_the_whole_batch_and_commits_nothing() {
    // The one run-level conflict there is. §4 offers no failure edge out of
    // `committing`, so the honest terminal state is `completed_with_conflicts`
    // with every row named — and, critically, nothing written.
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let (_, other_version) = seed_draft(&h, key("us"), 5_000).await;

    // A neighbour run holds the row this one wants.
    let neighbour = open_run(&h, "c-7").await;
    let conn = h.provider.conn().expect("conn");
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
        "and the refusal names the run holding it: {}",
        receipt.conflicted[0].violations[0].detail
    );
}

#[tokio::test]
async fn an_interactive_edit_is_refused_by_the_lock_and_told_which_run() {
    // `fr-concurrent-edit`'s other end, in the door rather than on a surface: a
    // rule that lives on one authoring path is not a rule, and this is the second
    // path onto the same rows.
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-10").await;
    let conn = h.provider.conn().expect("conn");
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("enter committing");
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[price_id], at(11))
        .await
        .expect("hold the row");

    let refused = h
        .prices
        .update_draft(
            &scope(),
            TENANT,
            price_id,
            version,
            content(12_500),
            stamp(),
            None,
        )
        .await
        .expect_err("an interactive edit belongs to no run");
    // The variant, as the delete-verb sibling asserts it: `update_draft` refuses
    // a stale version and a non-draft row too, and a rendering that happened to
    // carry the run id would satisfy a `contains` from either of them.
    assert!(
        matches!(refused, RepoError::BulkRowLocked { .. }),
        "the lock is what refused, not a neighbour: {refused:?}"
    );
    let rendered = format!("{refused:?}");
    assert!(
        rendered.contains(&run.to_string()),
        "the conflict names the run holding it: {rendered}"
    );
    // And the refused edit edited nothing.
    assert_eq!(
        h.prices
            .find(&scope(), TENANT, price_id)
            .await
            .expect("read the row back")
            .expect("the row is there")
            .row
            .amount_minor
            .map(bss_pricing::domain::money::MinorAmount::get),
        Some(9_900),
        "the seeded amount stands"
    );
}

#[tokio::test]
async fn the_run_holding_the_row_edits_it_freely() {
    // **The distinction the guard exists for, and the easy mistake.** Phase 2
    // edits the very rows it locked, so a guard refusing every locked row would
    // make the commit refuse its own batch. What the lock excludes is somebody
    // else.
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-11").await;
    let conn = h.provider.conn().expect("conn");
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("enter committing");
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[price_id], at(11))
        .await
        .expect("hold the row");

    h.prices
        .update_draft(
            &scope(),
            TENANT,
            price_id,
            version,
            content(12_500),
            stamp(),
            Some(run),
        )
        .await
        .expect("its own holder passes");
}

/// **And the same distinction on the delete verb — plus the second refusal
/// standing behind it** (Z7-10).
///
/// `delete_draft` hard-coded `on_behalf_of: None`, so the guard answered
/// `BULK_ROW_LOCKED` **naming the run making the call**. Fail-closed behind an
/// absent lane — neither bulk lane has a delete arm today (`commit_rows` has an
/// edit arm and a create arm; `infra::repricing` stages successors and creates
/// drafts) — which is exactly why it had no test and could not get one until the
/// parameter existed.
///
/// **Threading it is necessary and not sufficient, which this case is here to
/// record.** `fk_pricing_bulk_row_lock_price` references `pricing_price
/// (price_id)` with no cascade, so a held row cannot be deleted while its own
/// lock row stands: past the guard, the delete meets the foreign key and comes
/// back as `RepoError::Db` — a 500. So the lane that lands a delete verb owes
/// `bulk_repo::release_locks` for that row **inside the same transaction**, and
/// the third arm below is what says that is the whole remaining obstacle rather
/// than a wall. Filing this as a passing test rather than a fix, because giving a
/// price repository the power to drop another aggregate's lock row is a design
/// decision and not a repair.
///
/// The **`None` arm is the control**: without it, a `delete_draft` that had
/// simply stopped consulting the lock would satisfy the first arm, and the guard
/// would be gone rather than corrected.
#[tokio::test]
async fn the_lock_stops_naming_the_run_that_holds_it_when_that_run_deletes() {
    let h = harness().await;
    let (mine, my_version) = seed_draft(&h, key("eu"), 9_900).await;
    let (theirs, their_version) = seed_draft(&h, key("us"), 5_000).await;
    let run = open_run(&h, "c-14").await;
    let conn = h.provider.conn().expect("conn");
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({}),
        at(11),
    )
    .await
    .expect("enter committing");
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[mine, theirs], at(11))
        .await
        .expect("the run holds both rows");

    // Arm 1: the holder's own delete is past the guard. What refuses it now is
    // the foreign key, not `inst-bk-lock` — and the difference is the whole
    // finding, because one of the two names the caller's own run at it.
    let blocked = h
        .prices
        .delete_draft(&scope(), TENANT, mine, my_version, stamp(), Some(run))
        .await
        .expect_err("the lock row's foreign key still stands in the way");
    assert!(
        !matches!(blocked, RepoError::BulkRowLocked { .. }),
        "the guard must not refuse the run that holds the row: {blocked:?}"
    );
    assert!(
        format!("{blocked:?}").contains("FOREIGN KEY"),
        "and what does refuse is fk_pricing_bulk_row_lock_price: {blocked:?}"
    );

    // Arm 2, the control: somebody else — an interactive delete belongs to no
    // run — is still refused BY THE GUARD, and the refusal still names the
    // holder.
    let refused = h
        .prices
        .delete_draft(&scope(), TENANT, theirs, their_version, stamp(), None)
        .await
        .expect_err("an interactive delete belongs to no run");
    assert!(
        matches!(refused, RepoError::BulkRowLocked { .. }),
        "somebody else meets the guard, not the foreign key: {refused:?}"
    );
    assert!(
        format!("{refused:?}").contains(&run.to_string()),
        "the conflict names the run holding it: {refused:?}"
    );
    assert!(
        h.prices
            .find(&scope(), TENANT, theirs)
            .await
            .expect("read back")
            .is_some(),
        "and the refused delete deleted nothing"
    );

    // Arm 3: with the lock released, the run's own delete lands. This is the
    // shape the future lane owes — release, then delete, in one transaction.
    bulk_repo::release_locks(&conn, &scope(), TENANT, run)
        .await
        .expect("the run releases what it holds");
    h.prices
        .delete_draft(&scope(), TENANT, mine, my_version, stamp(), Some(run))
        .await
        .expect("and then the row goes");
    assert_eq!(
        h.prices
            .find(&scope(), TENANT, mine)
            .await
            .expect("read back"),
        None,
        "really gone, not merely un-refused"
    );
}

#[tokio::test]
async fn a_run_that_cannot_take_every_lock_releases_the_ones_it_took() {
    // **The leak the first arrangement hid** (D-294). The earlier case put the
    // contended row FIRST, so the very first insert collided and nothing partial
    // existed. Here it is second: the run takes one lock, collides on the next,
    // and the first must not be left held by an operation that is over — which is
    // the freeze `inst-bs-done`'s "lock released either way" exists to prevent.
    let h = harness().await;
    let (free_row, free_version) = seed_draft(&h, key("us"), 5_000).await;
    let (held_row, held_version) = seed_draft(&h, key("eu"), 9_900).await;

    let neighbour = open_run(&h, "c-12").await;
    let conn = h.provider.conn().expect("conn");
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
    bulk_repo::take_locks(&conn, &scope(), TENANT, neighbour, &[held_row], at(11))
        .await
        .expect("it holds the second row");

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
        bulk_repo::lock_holder(&conn, &scope(), TENANT, free_row)
            .await
            .expect("read the lock"),
        None,
        "the row this run did take must be free again"
    );

    // And nothing was written: the seeded amounts stand, **each row by name**.
    //
    // Not `< 9_901`. That bound was the defect: the free row is seeded 5_000 and
    // the run attempts 7_500, so a `commit_rows` that wrote the row whose lock it
    // *did* take before colliding on the next — the one row this case exists for
    // — leaves 7_500 in the store, and 7_500 satisfies the bound. Measured
    // 2026-08-20: seeding the free row at 7_500 (the residue's own value) left
    // the old assertion green.
    let rows = bss_pricing::infra::storage::repo::price_repo::load_for_plan(
        &conn,
        &scope(),
        TENANT,
        plan(),
        &[bss_pricing::domain::lifecycle::LifecycleState::Draft],
    )
    .await
    .expect("read the drafts");
    let mut stored: Vec<(Uuid, i64)> = rows
        .iter()
        .map(|record| {
            (
                record.price_id,
                record.row.amount_minor.expect("an amount").get(),
            )
        })
        .collect();
    stored.sort_by_key(|(price_id, _)| *price_id);
    let mut expected = vec![(free_row, 5_000_i64), (held_row, 9_900_i64)];
    expected.sort_by_key(|(price_id, _)| *price_id);
    assert_eq!(
        stored, expected,
        "no row moved: every draft still holds exactly the amount it was seeded with"
    );
}

#[tokio::test]
async fn the_stored_report_carries_both_halves() {
    // `inst-bk-idem` replays this report to a retry, so what the run *stores* is
    // the contract — not merely what `commit_batch` returned to its caller.
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

    let conn = h.provider.conn().expect("conn");
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

#[tokio::test]
async fn a_run_level_failure_keeps_the_rows_that_did_commit_in_the_report() {
    // **The report a failed run leaves has to be true of the store** (D-300).
    // Every row is its own transaction, so on a fault at row 1 the draft row 0
    // wrote is *in* `pricing_price`. The receipt used to be discarded here and
    // replaced wholesale by "every row un-attempted", so the run's stored report
    // denied a row that exists — and an operator who resubmits on that report
    // meets a stale `ETag` on every row it hid.
    //
    // The fault is a real one: `billing_timing` is a free `String` on this plane,
    // Slice 6 owns its vocabulary, and no Phase-1 rule screens it — so the column
    // CHECK is the first thing that sees it.
    let h = harness().await;
    let operation_id = open_run(&h, "the-run-fails-midway").await;

    let mut bad = row(key("us"), 2_500, None);
    bad.content.billing_timing = Some("whenever".to_owned());

    let failure = commit_batch(
        &h.provider,
        &h.prices,
        &scope(),
        TENANT,
        operation_id,
        &[row(key("eu"), 1_500, None), bad],
        stamp(),
        at(11),
    )
    .await
    .expect_err("a fault that is the run's still reaches the caller");
    assert!(
        failure.to_string().contains("billing_timing"),
        "and it names what went wrong: {failure}"
    );

    let conn = h.provider.conn().expect("conn");
    let run = bulk_repo::read(&conn, &scope(), TENANT, operation_id)
        .await
        .expect("read")
        .expect("exists");
    assert!(
        run.state.is_terminal(),
        "the run lands terminal whatever happened: {:?}",
        run.state
    );

    let committed = run.report["committed"]
        .as_array()
        .unwrap_or_else(|| panic!("the stored report's committed arm: {}", run.report));
    assert_eq!(
        committed.len(),
        1,
        "row 0 committed and the report has to say so: {}",
        run.report
    );
    assert_eq!(committed[0]["row"], serde_json::json!(0));

    // And the row it names is really there — which is what makes the old report a
    // false statement rather than a conservative one.
    let landed = committed[0]["price_id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .expect("a committed row names its draft");
    //
    // Scoped with `scope()`, not `allow_all()`: under `allow_all` the read matches
    // on `price_id` alone, so it cannot tell "the row landed for this tenant" from
    // "the row landed under the wrong tenant_id". The predicate is live here —
    // measured 2026-08-20 by pointing this same read at a foreign tenant, which
    // reddened it.
    let stored = price::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(Condition::all().add(price::Column::PriceId.eq(landed)))
        .one(&conn)
        .await
        .expect("read the row");
    assert!(
        stored.is_some(),
        "the report's committed row exists in the store, under this tenant: {}",
        run.report
    );
}

// ---------------------------------------------------------------------------
// Z8-8 — the two exits no match arm reaches.
// ---------------------------------------------------------------------------

/// **The RED this case is about.** `commit_batch` covered the `Err` exit
/// meticulously — D-294 took the `?` off `commit_rows` and D-300 took it off
/// `release_locks` — and covered neither of the other two ways the function can
/// end. A panic inside `commit_rows` unwinds *past* a match arm exactly as it
/// unwinds past everything else, and a dropped future (a client disconnect, a
/// shutdown signal, a losing `select!` arm) never runs any of this crate's code
/// again, match arm or not. Neither is rolled back, because `take_locks` requires
/// an autocommit connection **by design**: "Postgres aborts an enclosing
/// transaction on a failed statement".
///
/// What is left behind is the freeze `inst-bs-done`'s "lock released either way"
/// exists to prevent: the rows stay held against every interactive editor and the
/// run stays `committing` with no timeout, no sweeper, and D-37's lease takeover
/// designed and unbuilt.
///
/// Only [`Drop`] is the language's own guarantee across a panic and a
/// cancellation together, and the crate already has the precedent one module over
/// (`infra::repricing::RunLockGuard`, whose own doc names this very finding).
///
/// **Armed at an abnormal exit, not at an `Err`.** The task is genuinely
/// cancelled with `JoinHandle::abort` — `select!`'s own mechanism for a losing arm
/// — and only after the lock has been *observed* taken, because a future dropped
/// before `take_locks` ran would prove nothing about the guard. The polling loop
/// is what stops this case passing that way.
#[tokio::test]
async fn a_commit_future_dropped_mid_flight_releases_its_locks_and_lands_the_run_terminal() {
    let h = harness().await;

    // Enough rows that the per-row loop is still running when the abort lands:
    // every row is its own transaction with its own audit write, so this is the
    // margin the cancellation happens inside. They are **edits**, because only a
    // row whose draft already exists is a row `take_locks` has anything to lock.
    let mut rows = Vec::new();
    let mut price_ids = Vec::new();
    for index in 0..40_u32 {
        let region = format!("r{index}");
        let (price_id, version) = seed_draft(&h, key(&region), 9_900).await;
        price_ids.push(price_id);
        rows.push(row(key(&region), 12_500, Some(version)));
    }
    let run = open_run(&h, "c-drop-1").await;

    let provider = h.provider.clone();
    let prices = h.prices.clone();
    let handle = tokio::spawn(async move {
        commit_batch(
            &provider,
            &prices,
            &scope(),
            TENANT,
            run,
            &rows,
            stamp(),
            at(11),
        )
        .await
    });

    // Polled rather than slept a fixed amount, so this is not a bet on how fast
    // one test box is.
    let take_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let conn = h.provider.conn().expect("conn");
        let held = bulk_repo::lock_holder(&conn, &scope(), TENANT, price_ids[0])
            .await
            .expect("read the lock");
        if held == Some(run) {
            break;
        }
        assert!(
            std::time::Instant::now() < take_deadline,
            "the commit never took its row locks within 10s, and this case cannot say anything \
             about a drop guard without first observing the locks taken"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    handle.abort();
    match handle.await {
        Err(ref e) if e.is_cancelled() => {}
        other => panic!(
            "the task must have been genuinely cancelled mid-flight for this case to prove \
             anything, not merely finished before the abort landed: {other:?}"
        ),
    }

    // The fallback is a detached spawn — `Drop` cannot await — so it is not
    // synchronous with the abort. Polled, for the reason above.
    let settle_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let conn = h.provider.conn().expect("conn");
        let run_now = bulk_repo::read(&conn, &scope(), TENANT, run)
            .await
            .expect("read the run")
            .expect("the run exists");
        let mut held = None;
        for &price_id in &price_ids {
            if let Some(holder) = bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
                .await
                .expect("read the lock")
            {
                held = Some((price_id, holder));
                break;
            }
        }
        if held.is_none() && run_now.state != BulkState::Committing {
            assert!(
                run_now.state.is_terminal(),
                "and the run lands on one of the terminal states rather than somewhere no \
                 edge leaves: {run_now:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < settle_deadline,
            "10s after a cancelled commit the run is still {:?} and {held:?} still holds a \
             row: nothing releases the locks a dropped future left, and nothing lands the run",
            run_now.state
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// The **positive control** for the guard: a commit that ends normally must not
/// be swept by it. Without this, a guard that fired unconditionally — landing
/// every run `completed_with_conflicts` over the receipt's own terminal state —
/// would satisfy the case above.
///
/// # Why this control does not wait for the sweep, and cannot
///
/// It used to `sleep(50ms)` and call that "long enough that a detached sweep, if
/// one had been spawned, would have run" — a bet on one test box, beside a case
/// that polls the same condition for 10s for exactly that reason. But no window
/// would have helped: the sweep is [`abandon_committing_run`], whose landing is
/// `bulk_repo::advance(Committing -> CompletedWithConflicts)`, a compare-and-swap
/// — and a normally-completed run reads `Completed`. Measured 2026-08-20: firing
/// the sweep by hand over this very run answers
/// `ConcurrentMutation { .. "the move committing -> completed_with_conflicts
/// names a run that now reads completed" }`, having written neither the state nor
/// the `aborted` note (the note rides that same refused `advance`).
///
/// So a spurious fire is **inert**, and this control asserts that rather than
/// waiting for it: it runs precisely what `Drop` would have run and pins the
/// refusal. Deterministic, and it holds on a box of any speed.
#[tokio::test]
async fn a_commit_that_ends_normally_is_not_touched_by_the_guard() {
    let h = harness().await;
    let (_, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "c-drop-2").await;

    let receipt = run_phase_2(&h, run, &[row(key("eu"), 12_500, Some(version))]).await;
    assert_eq!(committed_rows(&receipt), vec![0]);

    // What the guard's `Drop` would have run, run here synchronously. It has to be
    // refused, and refused by the state machine rather than by luck of timing.
    let conn = h.provider.conn().expect("conn");
    let swept = abandon_committing_run(
        &conn,
        &scope(),
        TENANT,
        run,
        "a sweep that must not land over a run that finished",
        at(12),
    )
    .await
    .expect_err("a completed run is not committing, so the sweep's landing has no edge to take");
    assert!(
        matches!(swept, RepoError::ConcurrentMutation { .. }),
        "and it is refused as the concurrent mutation it is, not as a storage fault: {swept:?}"
    );

    let stored = bulk_repo::read(&conn, &scope(), TENANT, run)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(
        stored.state,
        BulkState::Completed,
        "the receipt's own terminal state stands, not the guard's fallback: {stored:?}"
    );
    assert!(
        stored.report.get("aborted").is_none(),
        "and no interruption note was stamped over a run that finished: {}",
        stored.report
    );
}

// ---------------------------------------------------------------------------
// Z9-5 — the sweep the guard cannot promise, and what a swept report does and
// does not say.
// ---------------------------------------------------------------------------

/// The run's stored report, whole.
async fn report_of(h: &Harness, operation_id: Uuid) -> serde_json::Value {
    let conn = h.provider.conn().expect("conn");
    bulk_repo::read(&conn, &scope(), TENANT, operation_id)
        .await
        .expect("read")
        .expect("exists")
        .report
}

/// A run left exactly where an abnormal exit leaves one and where **the guard
/// did not run**: `committing`, its row locks held, nothing in flight.
///
/// That state is not hypothetical and is the reason `POST …/abort` stays. The
/// guard's `Drop` fallback is a detached spawn, so it needs a live Tokio runtime
/// and a live process; a runtime that has gone away or a `kill -9` runs neither,
/// and its own doc says so. What is left over is exactly this row, and the sweep
/// below is the whole remedy for it.
async fn a_run_stuck_committing(h: &Harness, client_key: &str, price_id: Uuid) -> Uuid {
    let conn = h.provider.conn().expect("conn");
    let run = open_run(h, client_key).await;
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run,
        BulkState::Validating,
        BulkState::Committing,
        serde_json::json!({ "phase": "committing", "rows": 1 }),
        at(11),
    )
    .await
    .expect("the run enters committing exactly as `commit_batch` moves it");
    bulk_repo::take_locks(&conn, &scope(), TENANT, run, &[price_id], at(11))
        .await
        .expect("and takes its row locks there");
    run
}

/// **The half of Z9-5's probe the drop-guard case cannot reach**: "assert the
/// run is still recoverable through abort".
///
/// [`a_commit_future_dropped_mid_flight_releases_its_locks_and_lands_the_run_terminal`]
/// proves the guard sweeps a cancelled commit — but by sweeping it, it also means
/// no case ever reaches the door the guard's own doc names as the residue's
/// remedy. `abandon_committing_run` had **no success-path coverage anywhere in
/// the crate**: `rest_bulk_imports` exercises only its two refusals (a finished
/// run, a run that ended with conflicts), and the drop case asserts state and
/// locks without ever reading the report.
///
/// So this asserts the sweep as an *operator remedy*, which is three claims and
/// not one: the rows come unfrozen, the run leaves a state §4 gives it no edge
/// out of, and the report says an abort happened **without discarding what the
/// column already held**. The last is the one a state assertion cannot see and
/// the one D-300 wrote the "added to, never replaced" rule for.
#[tokio::test]
async fn a_run_no_guard_swept_is_recoverable_through_the_abort_sweep() {
    let h = harness().await;
    let (price_id, _) = seed_draft(&h, key("eu"), 9_900).await;
    let run = a_run_stuck_committing(&h, "c-stuck-1", price_id).await;

    let conn = h.provider.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
            .await
            .expect("read the lock"),
        Some(run),
        "the fixture has to be genuinely frozen, or this case proves nothing about thawing it"
    );

    abandon_committing_run(&conn, &scope(), TENANT, run, ABORT_NOTE, at(12))
        .await
        .expect("the sweep is the abort route's whole body");

    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
            .await
            .expect("read the lock"),
        None,
        "the interactive editors get their row back"
    );
    let state = state_of(&h, run).await;
    assert_eq!(
        state,
        BulkState::CompletedWithConflicts,
        "and the run leaves `committing`, which section 4 gives it no failure edge out of"
    );
    let report = report_of(&h, run).await;
    assert_eq!(
        report
            .get(ABORTED_MEMBER)
            .and_then(serde_json::Value::as_str),
        Some(ABORT_NOTE),
        "the note is stamped where an operator reads it: {report}"
    );
    assert_eq!(
        report.get("rows").and_then(serde_json::Value::as_u64),
        Some(1),
        "and it is added to the report rather than replacing it, so what the column already \
         held survives the sweep: {report}"
    );
}

/// **The note survives a report that is not a JSON object** (C5-1).
///
/// `abandon_committing_run` stamped its note through `report.as_object_mut()`
/// and did nothing at all when that answered `None`: the sweep still landed the
/// run terminal, so an operator reading it could not tell an abort from an
/// ordinary completion, and the note is the only evidence the sweep ran. Every
/// current writer stores an object — `report_of` serializes a `CommitReceipt` —
/// so this is robustness rather than a live defect, which is exactly why it
/// needed a case: nothing else can reach the arm.
///
/// Refusing instead would be wrong. This sweep *is* the remedy for a run stuck
/// `committing`, and by the time the note is stamped the locks are about to be
/// released; a refusal would leave the run with no door out for the sake of a
/// malformed column.
#[tokio::test]
async fn an_abort_note_is_kept_even_when_the_stored_report_is_not_an_object() {
    let h = harness().await;
    let (price_id, _) = seed_draft(&h, key("eu"), 9_900).await;
    let run = a_run_stuck_committing(&h, "c-stuck-3", price_id).await;

    let conn = h.provider.conn().expect("conn");
    // A shape no current writer produces, put there directly. `advance` is the
    // only writer of this column and it takes whatever JSON it is handed.
    bulk_repo::advance(
        &conn,
        &scope(),
        TENANT,
        run,
        BulkState::Committing,
        BulkState::Committing,
        serde_json::json!("a report that is not an object"),
        at(11),
    )
    .await
    .expect("seed the malformed report");

    abandon_committing_run(&conn, &scope(), TENANT, run, ABORT_NOTE, at(12))
        .await
        .expect("the sweep must still land: it is the remedy, not a validator");

    let report = report_of(&h, run).await;
    assert_eq!(
        report
            .get(ABORTED_MEMBER)
            .and_then(serde_json::Value::as_str),
        Some(ABORT_NOTE),
        "the note is where an operator reads it whatever the column held: {report}"
    );
    assert_eq!(
        report
            .get(PRIOR_REPORT_MEMBER)
            .and_then(serde_json::Value::as_str),
        Some("a report that is not an object"),
        "and `added to, never replaced` holds on the one shape with no member to \
         add to: the prior value moves rather than being discarded: {report}"
    );
    assert_eq!(
        state_of(&h, run).await,
        BulkState::CompletedWithConflicts,
        "and the run still leaves `committing`"
    );
}

/// The premise the sweep is only correct under is judged **by the statement**,
/// not by a read in front of it.
///
/// A second sweep of a run the first already landed must not rewrite
/// `completed_at` and stamp a second note over a report whose rows were all
/// attempted — the exact hazard `abort_bulk_import`'s state guard was written for
/// when D-293 wrongly claimed the trigger refused it. Here the refusal is the
/// `Committing -> …` premise riding into the `UPDATE`, which is what makes the
/// sweep safe to retry and safe to race with the guard's detached fallback.
#[tokio::test]
async fn a_second_sweep_of_a_landed_run_is_refused_by_the_statements_own_premise() {
    let h = harness().await;
    let (price_id, _) = seed_draft(&h, key("eu"), 9_900).await;
    let run = a_run_stuck_committing(&h, "c-stuck-2", price_id).await;

    let conn = h.provider.conn().expect("conn");
    abandon_committing_run(&conn, &scope(), TENANT, run, ABORT_NOTE, at(12))
        .await
        .expect("the first sweep lands it");

    let second = abandon_committing_run(&conn, &scope(), TENANT, run, "a second note", at(13))
        .await
        .expect_err("a run that is over has no locks to clear and no work to stop");

    assert!(
        matches!(
            second,
            bss_pricing::infra::storage::RepoError::ConcurrentMutation { .. }
        ),
        "got: {second:?}"
    );
    let report = report_of(&h, run).await;
    assert_eq!(
        report
            .get(ABORTED_MEMBER)
            .and_then(serde_json::Value::as_str),
        Some(ABORT_NOTE),
        "and the first sweep's note is not overwritten by the refused one: {report}"
    );
}

/// **The recorded residual, made visible rather than papered over.**
///
/// A cancelled commit lands terminal carrying only the report `commit_batch`
/// wrote *on entry* — how many rows the run was about — because the receipt that
/// would have listed what committed died with the future. So the rows this run
/// did commit are in the store and are **not** in its report, and the note is the
/// only thing that says so.
///
/// This case exists to pin that as a known state rather than let a later reader
/// discover it as a surprise: it drives the cancellation only after observing a
/// row actually committed, so "the report omits what the store holds" is asserted
/// about a run where the omission is real. `INTERRUPTED_NOTE` is what carries the
/// remedy ("re-read the rows and resubmit whatever is still owed"), which is why
/// the note's presence is asserted beside the omission rather than instead of it.
#[tokio::test]
async fn a_cancelled_commits_report_says_it_was_interrupted_and_not_what_it_committed() {
    let h = harness().await;

    let mut rows = Vec::new();
    let mut price_ids = Vec::new();
    for index in 0..40_u32 {
        let region = format!("s{index}");
        let (price_id, version) = seed_draft(&h, key(&region), 9_900).await;
        price_ids.push(price_id);
        rows.push(row(key(&region), 12_500, Some(version)));
    }
    let run = open_run(&h, "c-residual-1").await;

    let provider = h.provider.clone();
    let prices = h.prices.clone();
    let handle = tokio::spawn(async move {
        commit_batch(
            &provider,
            &prices,
            &scope(),
            TENANT,
            run,
            &rows,
            stamp(),
            at(11),
        )
        .await
    });

    // Wait for a row to have **actually committed**, not merely for the locks to
    // be taken: the claim below is about rows the store holds and the report does
    // not, and a cancellation landing before the first row would make it vacuous.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if amount_of(&h, price_ids[0]).await == Some(12_500) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no row committed within 10s, so this case cannot say anything about a report that \
             omits committed rows"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }

    handle.abort();
    match handle.await {
        Err(ref e) if e.is_cancelled() => {}
        other => panic!("the task must have been genuinely cancelled mid-flight: {other:?}"),
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let report = loop {
        let report = report_of(&h, run).await;
        if report.get(ABORTED_MEMBER).is_some() {
            break report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "10s after a cancelled commit nothing stamped an interruption note, so an operator \
             reading this run cannot tell it was interrupted at all: {report}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };

    let note = report
        .get(ABORTED_MEMBER)
        .and_then(serde_json::Value::as_str)
        .expect("the note is a string");
    assert!(
        note.contains("interrupted") && note.contains("resubmit"),
        "the note has to say what happened and what is owed, because the report cannot: {note}"
    );
    assert!(
        note != ABORT_NOTE,
        "and it is the guard's note, not the abort route's: an operator who did not press abort \
         must not read that somebody did"
    );

    // The residual itself. Recorded, not papered over: the store holds a row this
    // run committed and the report names none of them.
    assert_eq!(
        amount_of(&h, price_ids[0]).await,
        Some(12_500),
        "the row committed and stands"
    );
    assert!(
        report.get("committed").is_none(),
        "and the report lists nothing it committed - it carries only what `commit_batch` wrote \
         on entry, because the receipt died with the future. This is the residual Z9-5 records; \
         the note above is the whole of what an operator is given instead: {report}"
    );
}

/// One draft row's authored amount, or `None` when the row is not there.
async fn amount_of(h: &Harness, price_id: Uuid) -> Option<i64> {
    let conn = h.provider.conn().expect("conn");
    price::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .one(&conn)
        .await
        .expect("read the row")
        .and_then(|row| row.amount_minor)
}

// ---------------------------------------------------------------------------
// Z11-10 — the row that failed is not the rows that were never reached.
// ---------------------------------------------------------------------------

/// **The RED this case is about.** On a run-level fault at row `k` the loop ran
/// `for unreached in k..rows.len()` and stamped every one of them "not attempted:
/// the run failed at row {k}" — including row `k` itself, which **was** attempted
/// and whose own transaction is the thing that failed. Including it in the receipt
/// is right (nothing committed for it); the sentence was not.
///
/// The distinction is the operator's next action. A row that was never reached is
/// resubmitted unchanged; the row that failed is the one whose content they have to
/// look at, and it is the only row in the receipt the failure sentence is actually
/// about. `not_attempted`'s own doc records this same family being corrected once
/// before, for putting the contended row's `price_id` on every row's violation.
///
/// **Three rows, and the middle one fails**, because that is the only arrangement
/// that can tell the two sentences apart: with the fault on the last row a
/// green-looking `k + 1..len` is an empty range either way. `billing_timing` is a
/// free `String` on this plane whose column CHECK is the first thing that sees it,
/// which is `a_run_level_failure_keeps_the_rows_that_did_commit_in_the_report`'s
/// fault injection and a genuinely run-level one.
#[tokio::test]
async fn the_row_whose_own_transaction_failed_is_not_reported_as_not_attempted() {
    let h = harness().await;
    let operation_id = open_run(&h, "the-middle-row-fails").await;

    let mut bad = row(key("us"), 2_500, None);
    bad.content.billing_timing = Some("whenever".to_owned());

    let failure = commit_batch(
        &h.provider,
        &h.prices,
        &scope(),
        TENANT,
        operation_id,
        &[
            row(key("eu"), 1_500, None),
            bad,
            row(key("apac"), 3_500, None),
        ],
        stamp(),
        at(11),
    )
    .await
    .expect_err("a fault that is the run's still reaches the caller");
    assert!(
        failure.to_string().contains("billing_timing"),
        "the premise of this case: the fault is the run's and it is row 1's own: {failure}"
    );

    let conn = h.provider.conn().expect("conn");
    let report = bulk_repo::read(&conn, &scope(), TENANT, operation_id)
        .await
        .expect("read")
        .expect("exists")
        .report;

    // Row 0 committed, which is what makes the other two claims about a partial
    // result rather than about a run that did nothing.
    let committed: Vec<u64> = report["committed"]
        .as_array()
        .unwrap_or_else(|| panic!("the stored report's committed arm: {report}"))
        .iter()
        .filter_map(|row| row["row"].as_u64())
        .collect();
    assert_eq!(committed, vec![0], "row 0 committed: {report}");

    let detail_of = |row: u64| -> String {
        let outcome = report["conflicted"]
            .as_array()
            .unwrap_or_else(|| panic!("the conflicted arm: {report}"))
            .iter()
            .find(|outcome| outcome["row"].as_u64() == Some(row))
            .unwrap_or_else(|| panic!("row {row} has to be in the report at all: {report}"));
        outcome["violations"][0]["detail"].to_string()
    };

    let failed = detail_of(1);
    assert!(
        !failed.contains("not attempted"),
        "row 1 WAS attempted - its own transaction failed, and that is the one thing its \
         sentence must not deny: {failed}"
    );
    assert!(
        failed.contains("billing_timing"),
        "and its sentence names the failure, which is what an operator has to act on: {failed}"
    );

    // The positive control for the sentence above: the row after it really was
    // never reached, and still says so. A fix that dropped the "not attempted"
    // sentence altogether would pass the two assertions above and lose the
    // distinction they exist to draw.
    let unreached = detail_of(2);
    assert!(
        unreached.contains("not attempted"),
        "row 2 was never reached and the report still has to say so: {unreached}"
    );

    // And the store agrees with both halves: row 1 authored nothing, so "nothing
    // was committed for it" is a fact about `pricing_price` and not only a
    // sentence in a report.
    let authored: Vec<i64> = price::Entity::find()
        .secure()
        .scope_with(&scope())
        .all(&conn)
        .await
        .expect("read the rows")
        .into_iter()
        .filter_map(|row| row.amount_minor)
        .collect();
    assert_eq!(
        authored,
        vec![1_500],
        "only row 0's amount is in the store: rows 1 and 2 committed nothing"
    );
}

// ---------------------------------------------------------------------------
// H9 — the release that did not happen.
// ---------------------------------------------------------------------------

/// A `BEFORE DELETE` trigger on `pricing_bulk_row_lock`, and nothing else.
///
/// **The fault has to be on the DELETE alone.** What the two cases below are
/// about is the ordering of `release_locks` against the terminal `advance`, so a
/// fault that also broke `pricing_bulk_operation` would land the run somewhere
/// for a second reason and neither case could tell which statement had done it.
/// A trigger is the narrowest injection point there is: `release_locks` is a
/// `delete_many` on this one table, no seam of any kind enters the crate, and
/// every other statement of the run is left healthy.
struct RefuseEveryLockRelease;

impl MigrationName for RefuseEveryLockRelease {
    fn name(&self) -> &'static str {
        "m99999999_000001_test_refuse_every_lock_release"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for RefuseEveryLockRelease {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_raw(Statement::from_string(
                manager.get_database_backend(),
                "CREATE TRIGGER trg_test_refuse_every_lock_release
                 BEFORE DELETE ON pricing_bulk_row_lock
                 FOR EACH ROW
                 BEGIN
                   SELECT RAISE(ABORT,
                     'injected: pricing_bulk_row_lock refuses every DELETE');
                 END"
                .to_owned(),
            ))
            .await
            .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

/// Install [`RefuseEveryLockRelease`] on the harness's own database.
async fn refuse_every_lock_release(h: &Harness) {
    let applied =
        run_migrations_for_testing(&h.provider.db(), vec![Box::new(RefuseEveryLockRelease)])
            .await
            .expect("install the release refusal");
    assert_eq!(applied.applied, 1, "the fault must actually be installed");
}

/// Run Phase 2 and hand back whatever it answered.
///
/// [`run_phase_2`]'s sibling: these two cases are about the failure the run
/// reports, so the `expect` that helper carries would swallow the subject.
async fn try_phase_2(
    h: &Harness,
    operation_id: Uuid,
    rows: &[ImportRow],
) -> Result<CommitReceipt, bss_pricing::domain::error::DomainError> {
    commit_batch(
        &h.provider,
        &h.prices,
        &scope(),
        TENANT,
        operation_id,
        rows,
        stamp(),
        at(11),
    )
    .await
}

/// **A release that failed used to land the run terminal anyway, and that froze
/// its rows forever** (review H9, 2026-08-20).
///
/// `release_locks` then `advance` then `disarm` ran in that order with the first
/// one's failure carried as a value, so the run reached `completed` over locks
/// that were never deleted. Nothing can clear them from there: the abort route
/// refuses a run that is not `committing` (`LIFECYCLE_FORBIDDEN` — "a run that is
/// over has no locks to clear and no work to stop"), `pricing_bulk_row_lock` has
/// no sweeper, and D-37's lease takeover is unbuilt. Every row of the batch stays
/// refused to interactive editing and to every later bulk run, named by a run that
/// is over.
///
/// So the landing stops at the failed release: the run stays `committing`, which is
/// the one state the remedy is reachable from — [`abandon_committing_run`]'s own
/// ordering, and its `?` on the same call.
///
/// To redden this: put the terminal `advance` and `lock_guard.disarm()` back on
/// this path (`failed.or_else(|| released.err())`), and the run answers
/// `completed` with its lock still held.
#[tokio::test]
async fn a_release_that_fails_leaves_the_run_committing_so_the_abort_door_can_retry() {
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "release-fails-1").await;
    refuse_every_lock_release(&h).await;

    // An **edit**, because only a row whose draft already exists is a row
    // `take_locks` has anything to lock — and only a lock row that exists makes a
    // `BEFORE DELETE` trigger fire at all.
    let failure = try_phase_2(&h, run, &[row(key("eu"), 12_500, Some(version))])
        .await
        .expect_err("a release that did not happen is the run's failure");
    assert!(
        failure.to_string().contains("pricing_bulk_row_lock"),
        "and the caller is told which statement failed: {failure}"
    );

    assert_eq!(
        state_of(&h, run).await,
        BulkState::Committing,
        "a run whose locks are still held must stay in the only state they can be \
         released from"
    );
    let conn = h.provider.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
            .await
            .expect("read the lock"),
        Some(run),
        "the lock is really still held - which is what makes the state above the \
         honest one rather than a pessimistic one"
    );

    // And the door is open: the abort route's guard is the run's state, so the
    // sweep the operator reaches is applicable here. It is refused only because
    // the injected fault is still in place, and it names that fault rather than
    // the lifecycle.
    let refused = abandon_committing_run(&conn, &scope(), TENANT, run, ABORT_NOTE, at(12))
        .await
        .expect_err("the injected fault refuses the retry too");
    assert!(
        format!("{refused:?}").contains("pricing_bulk_row_lock"),
        "the retry reaches the release and fails there, not on the run's state: {refused:?}"
    );
}

/// **And the discarded error itself**: `failed.or_else(|| released.err())` never
/// called its closure when the run had already failed, so a failed release was
/// dropped on the floor entirely (review H9).
///
/// The two faults are independent and both real: row 1's `billing_timing` is a
/// value only the column CHECK sees (`a_run_level_failure_keeps_the_rows_that_did_
/// commit_in_the_report`'s own fault, reused), and the release refusal is the
/// injected trigger. Today the caller hears about the first and nothing at all
/// about the second, while the run lands terminal over a lock it still holds.
///
/// To redden this: replace `unreleased_landing` with `failed`, and the assertion on
/// `pricing_bulk_row_lock` fails while every other assertion here still passes —
/// which is the point, since the run-level failure was never the half that went
/// missing.
#[tokio::test]
async fn a_release_that_fails_alongside_a_failed_run_reports_both_and_still_holds_the_run() {
    let h = harness().await;
    let (price_id, version) = seed_draft(&h, key("eu"), 9_900).await;
    let run = open_run(&h, "release-fails-2").await;
    refuse_every_lock_release(&h).await;

    let mut bad = row(key("us"), 2_500, None);
    bad.content.billing_timing = Some("whenever".to_owned());

    let failure = try_phase_2(&h, run, &[row(key("eu"), 12_500, Some(version)), bad])
        .await
        .expect_err("both faults are the run's");
    let message = failure.to_string();
    assert!(
        message.contains("billing_timing"),
        "the run-level fault still reaches the caller: {message}"
    );
    assert!(
        message.contains("pricing_bulk_row_lock"),
        "and so does the release that did not happen, which `or_else` swallowed: {message}"
    );

    assert_eq!(
        state_of(&h, run).await,
        BulkState::Committing,
        "and the run is left where its locks can still be released"
    );
    let conn = h.provider.conn().expect("conn");
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, price_id)
            .await
            .expect("read the lock"),
        Some(run),
        "the lock the DELETE could not remove"
    );
}

/// **A `take_locks` that fails *after* an insert landed must not let the run go
/// terminal.** The release inside `take_locks` runs after the holder read, on
/// purpose (a release-first order deletes this run's own rows), so its failure
/// leaves behind exactly the lock rows the run already took.
///
/// `commit_batch` read every non-`BulkRowLocked` refusal as "no row was reached",
/// landed the run terminal and disarmed the guard. From there nothing releases
/// those rows: `abandon_committing_run` refuses a run that is not `committing`,
/// `pricing_bulk_row_lock` has no sweeper, and D-37's lease takeover is unbuilt —
/// so every row of the batch is frozen against interactive editing and against
/// every later bulk run, named by an operation that is over.
///
/// To redden this: put a bare `?` back on either statement of `take_locks`'
/// refusal path, and the run answers a terminal state over a lock it still holds.
#[tokio::test]
async fn a_lock_fault_that_may_have_left_rows_leaves_the_run_committing() {
    let h = harness().await;
    // Seeded in this order because `commit_batch` locks its targets by ascending
    // `price_id` and `Uuid::now_v7` is time-ordered: the free row is taken first,
    // so the collision on the second row happens with one lock already held. That
    // partial hold is the whole subject, and the assertion below proves it existed.
    let (free_row, free_version) = seed_draft(&h, key("us"), 5_000).await;
    let (held_row, held_version) = seed_draft(&h, key("eu"), 9_900).await;

    let neighbour = open_run(&h, "lock-fault-neighbour").await;
    let conn = h.provider.conn().expect("conn");
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
    bulk_repo::take_locks(&conn, &scope(), TENANT, neighbour, &[held_row], at(11))
        .await
        .expect("the neighbour holds the second row");

    // Installed after the neighbour has its lock, so the only DELETE the fault
    // meets is the one `take_locks` makes on its own partial set.
    refuse_every_lock_release(&h).await;

    let run = open_run(&h, "lock-fault-1").await;
    let failure = try_phase_2(
        &h,
        run,
        &[
            row(key("us"), 7_500, Some(free_version)),
            row(key("eu"), 12_500, Some(held_version)),
        ],
    )
    .await
    .expect_err("a lock set that may still be held is the run's failure");

    assert_eq!(
        state_of(&h, run).await,
        BulkState::Committing,
        "a run whose locks may still be held must stay in the only state \
         `POST /bulk-imports/{{id}}/abort` can act from"
    );
    assert_eq!(
        bulk_repo::lock_holder(&conn, &scope(), TENANT, free_row)
            .await
            .expect("read the lock"),
        Some(run),
        "and the hold is real: this run took this row's lock and the release could \
         not remove it, which is what makes the state above the honest one"
    );
    let message = failure.to_string();
    assert!(
        message.contains("locks may still be held"),
        "the caller is told what state the run is in, not merely that a statement \
         failed: {message}"
    );
}
