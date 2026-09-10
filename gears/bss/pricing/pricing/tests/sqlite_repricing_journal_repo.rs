//! The repricing journal's **repository** surface, executed (D-307).
//!
//! `sqlite_repricing_journal_store.rs` drives the table with raw SQL and is the
//! right shape for a constraint suite — a repository only ever writes legal
//! values, so a suite built on one catches a constraint that got *narrower* and
//! never one that stopped refusing. This file is its complement and asks the other
//! question: does the code that will drive the engine actually reach the table,
//! and does the table answer it the way the engine will need.
//!
//! It exists because the table had a schema, two behavioural suites and **no
//! caller** — the class this program keeps finding, and the reason
//! `trg_pricing_repricing_journal_only_under_a_repricing_run` was a rule nothing
//! could violate.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::bulk::{BulkKind, BulkState, JournalState};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::repricing_journal_repo::NewJournalRow;
use bss_pricing::infra::storage::repo::{
    IdempotencyGate, NewBulkOperation, NewPriceDraft, PriceRepo, bulk_repo, repricing_journal_repo,
};
use time::OffsetDateTime;

use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_51);
const ACTOR: Uuid = Uuid::from_u128(0xac_50);

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 10, hour, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

async fn provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

/// A real price row, because both of the journal's price columns are foreign
/// keys: a journal row naming a price that does not exist would be pending for
/// ever, and the run's completion predicate — no `pending` rows remain — would
/// never be satisfiable.
async fn a_price_row(p: &DBProvider<DbError>, region: &str) -> Uuid {
    let price_id = Uuid::now_v7();
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceRepo::new(p.clone())
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: ScopeKey::new(
                    PlanId::new(Uuid::from_u128(0x50_d5)),
                    CurrencyCode::new("EUR").expect("three letters"),
                    Region::new(region).expect("a non-blank region"),
                    PhaseId::new(Uuid::from_u128(0xfa_90)),
                    PriceEligibility::AllSubscriptions,
                    ChargeKind::Recurring,
                    Cohort::None,
                )
                .expect("the class pairs with the cohort"),
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: None,
                    rounding_policy_ref: Some("half_up".to_owned()),
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: ACTOR,
                created_at_utc: at(10),
                correlation_id: Uuid::from_u128(0xc0_51),
            },
        )
        .await
        .expect("author the row");
    price_id
}

async fn a_run(p: &DBProvider<DbError>, kind: BulkKind, client_key: &str) -> Uuid {
    let conn = p.conn().expect("conn");
    bulk_repo::open(
        &conn,
        &scope(),
        NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: TENANT,
            kind,
            client_key: client_key.to_owned(),
            request_hash: IdempotencyGate::payload_hash(client_key),
            report: serde_json::json!({ "rows": [] }),
            submitted_by: ACTOR,
            submitted_at: at(10),
        },
    )
    .await
    .expect("open the run")
    .operation_id
}

#[tokio::test]
async fn a_runs_row_set_is_frozen_pending_and_reads_back_in_price_order() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = a_run(&p, BulkKind::Repricing, "run-1").await;
    let first = a_price_row(&p, "eu").await;
    let second = a_price_row(&p, "us").await;

    repricing_journal_repo::open_rows(
        &conn,
        &scope(),
        &[
            NewJournalRow {
                run_id: run,
                price_id: second,
                tenant_id: TENANT,
            },
            NewJournalRow {
                run_id: run,
                price_id: first,
                tenant_id: TENANT,
            },
        ],
    )
    .await
    .expect("freeze the row set");

    let journal = repricing_journal_repo::list_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read the journal");
    assert_eq!(journal.len(), 2);
    assert!(
        journal.iter().all(|row| row.state == JournalState::Pending),
        "every row is born pending, whatever order it was written in: {journal:?}"
    );
    // Submitted second-then-first and read back in `price_id` order, which is
    // what makes a report rendered from this stable across reads.
    let mut expected = vec![first, second];
    expected.sort_unstable();
    assert_eq!(
        journal.iter().map(|row| row.price_id).collect::<Vec<_>>(),
        expected
    );
}

#[tokio::test]
async fn the_journal_refuses_a_row_under_an_import() {
    // `trg_pricing_repricing_journal_only_under_a_repricing_run`, which had no
    // production path able to violate it until this repository existed. An
    // import's per-row outcomes live in the operation's `report`, so a journal row
    // under one is a record no code will ever complete.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let import = a_run(&p, BulkKind::Import, "run-2").await;
    let repricing = a_run(&p, BulkKind::Repricing, "run-3").await;
    let price_id = a_price_row(&p, "eu").await;
    let other_price = a_price_row(&p, "us").await;

    // **Two rows, the legitimate one first, and inside a transaction.** A one-row
    // batch makes the readback below unfalsifiable: delete the trigger and the
    // `expect_err` panics before it, and nothing else in the case writes a journal.
    // A second, admissible row ahead of the refused one is what gives the readback
    // something to find - and it has to run under a transaction because
    // `open_rows` is a loop of inserts that opens none by design and says so: the
    // atomicity is the caller's, and a rollback is the release.
    let (_, outcome) = p
        .db()
        .in_transaction::<(), RepoError, _>({
            let scope = scope();
            move |txn| {
                Box::pin(async move {
                    repricing_journal_repo::open_rows(
                        txn,
                        &scope,
                        &[
                            NewJournalRow {
                                run_id: repricing,
                                price_id: other_price,
                                tenant_id: TENANT,
                            },
                            NewJournalRow {
                                run_id: import,
                                price_id,
                                tenant_id: TENANT,
                            },
                        ],
                    )
                    .await
                })
            }
        })
        .await;
    let refused = outcome.expect_err("a journal row under an import must be refused");
    // The trigger's own sentence. `contains("repricing")` said nothing: the table
    // is `pricing_repricing_journal` and every guard over it prefixes its message
    // with that name, so the assertion held for the foreign key, for the
    // append-only trigger, and for any CHECK this insert could have tripped
    // instead.
    assert!(
        refused
            .to_string()
            .contains("only a repricing run journals per-row progress"),
        "and the refusal says which kind it wanted: {refused}"
    );

    // And nothing was journalled **for either run**: the legitimate row went back
    // with the refused one, which is what "all or none" means and what a single-row
    // batch could not show.
    for (run, name) in [(import, "import"), (repricing, "repricing")] {
        assert!(
            repricing_journal_repo::list_for_run(&conn, &scope(), TENANT, run)
                .await
                .expect("read the run's journal")
                .is_empty(),
            "a refused open leaves the {name} run with no journal rows"
        );
    }
}

#[tokio::test]
async fn a_decided_row_is_final_and_the_re_drive_sees_only_what_is_left() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = a_run(&p, BulkKind::Repricing, "run-3").await;
    let applied = a_price_row(&p, "eu").await;
    let failed = a_price_row(&p, "us").await;
    let untouched = a_price_row(&p, "apac").await;
    let successor = a_price_row(&p, "successor").await;

    repricing_journal_repo::open_rows(
        &conn,
        &scope(),
        &[
            NewJournalRow {
                run_id: run,
                price_id: applied,
                tenant_id: TENANT,
            },
            NewJournalRow {
                run_id: run,
                price_id: failed,
                tenant_id: TENANT,
            },
            NewJournalRow {
                run_id: run,
                price_id: untouched,
                tenant_id: TENANT,
            },
        ],
    )
    .await
    .expect("freeze the row set");

    repricing_journal_repo::mark_applied(&conn, &scope(), TENANT, run, applied, successor, at(11))
        .await
        .expect("apply one");
    repricing_journal_repo::mark_failed(
        &conn,
        &scope(),
        TENANT,
        run,
        failed,
        "the plan-level pass refused",
    )
    .await
    .expect("fail another");

    // **The re-drive's whole safety.** A crashed run resumed by asking for
    // `pending` rows cannot apply anything twice, and cannot skip a row it never
    // reached — which is the one still here.
    let left = repricing_journal_repo::pending_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read what is left");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].price_id, untouched);

    let journal = repricing_journal_repo::list_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read the journal");
    let applied_row = journal
        .iter()
        .find(|row| row.price_id == applied)
        .expect("the applied row");
    assert_eq!(applied_row.state, JournalState::Applied);
    assert_eq!(applied_row.applied_price_id, Some(successor));
    assert_eq!(applied_row.applied_at, Some(at(11)));
    let failed_row = journal
        .iter()
        .find(|row| row.price_id == failed)
        .expect("the failed row");
    assert_eq!(failed_row.state, JournalState::Failed);
    assert_eq!(
        failed_row.failure_reason.as_deref(),
        Some("the plan-level pass refused"),
        "and the reason it carries is the one an operator reads"
    );
}

#[tokio::test]
async fn a_second_run_under_one_client_key_is_refused_per_kind_and_not_across_kinds() {
    // D-307. Section 5 gives the two flows two different idempotency columns, so
    // one key may open one import **and** one repricing run — and still only one
    // of each. Without the `kind` half of the index the second call here is
    // refused, and without the `kind` half of the query the import's replay would
    // answer with the repricing run.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let import = a_run(&p, BulkKind::Import, "shared").await;
    let repricing = a_run(&p, BulkKind::Repricing, "shared").await;
    assert_ne!(import, repricing);

    let found_import =
        bulk_repo::find_by_client_key(&conn, &scope(), TENANT, BulkKind::Import, "shared")
            .await
            .expect("read by key")
            .expect("the import is there");
    assert_eq!(found_import.operation_id, import);
    assert_eq!(found_import.kind, BulkKind::Import);

    let found_run =
        bulk_repo::find_by_client_key(&conn, &scope(), TENANT, BulkKind::Repricing, "shared")
            .await
            .expect("read by key")
            .expect("the run is there");
    assert_eq!(found_run.operation_id, repricing);

    // And uniqueness is kept **within** a kind, which is what `inst-bs-reject`'s
    // auditability argument rests on: a rejected run's key stays spent.
    let second_import = bulk_repo::open(
        &conn,
        &scope(),
        NewBulkOperation {
            operation_id: Uuid::now_v7(),
            tenant_id: TENANT,
            kind: BulkKind::Import,
            client_key: "shared".to_owned(),
            request_hash: IdempotencyGate::payload_hash("shared"),
            report: serde_json::json!({ "rows": [] }),
            submitted_by: ACTOR,
            submitted_at: at(10),
        },
    )
    .await;
    // The variant and what it names, not `is_err()`: any CHECK, either foreign key
    // or a scope failure satisfies the bare form, while the repository maps this
    // one collision to a typed `ConcurrentMutation` whose aggregate carries the two
    // axes of the key the caller can act on.
    let refused = second_import.expect_err("one key opens one run per kind, never two");
    let RepoError::ConcurrentMutation { ref aggregate } = refused else {
        panic!("a second run under one key is a contention, not a storage fault: {refused:?}");
    };
    assert!(
        aggregate.contains("import") && aggregate.contains("shared"),
        "and it names the kind and the client key that collided: {aggregate}"
    );
    assert_eq!(found_run.state, BulkState::Validating);
}

#[tokio::test]
async fn marking_a_row_that_is_not_in_the_journal_is_refused() {
    // Z8-2: `mark_applied`/`mark_failed` used to end `.exec(runner).await?; Ok(())`
    // without reading `rows_affected`, so a swap that matched no row answered
    // success — and the re-drive, which trusts a decided row to stay decided,
    // would apply the same row a second time.
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = Uuid::now_v7();
    // No journal row is opened for this pair at all.
    let outcome = repricing_journal_repo::mark_applied(
        &conn,
        &scope(),
        TENANT,
        run,
        Uuid::now_v7(),
        Uuid::now_v7(),
        at(11),
    )
    .await;
    assert!(
        matches!(outcome, Err(RepoError::NotFound { .. })),
        "a swap that matched no row must refuse, not answer Ok(()) — the re-drive \
         would otherwise apply the row a second time; got {outcome:?}"
    );

    let outcome = repricing_journal_repo::mark_failed(
        &conn,
        &scope(),
        TENANT,
        run,
        Uuid::now_v7(),
        "no row to fail",
    )
    .await;
    assert!(
        matches!(outcome, Err(RepoError::NotFound { .. })),
        "same refusal for the failure path; got {outcome:?}"
    );
}

/// **A row another pass already decided is a contention, not a store failure.**
///
/// `mark_applied`'s doc lists three things its swap refuses — the wrong
/// `(run_id, price_id)` pair, a row the scope gate filtered out, and *"a row
/// already decided"*. The third was false. `row_of` is three equalities and no
/// state conjunct, so a decided row **matches** the predicate; what stopped it was
/// `trg_pricing_repricing_journal_decided_is_final`, whose `RAISE(ABORT, ..)`
/// reaches the caller as `RepoError::Db` — a 500 — rather than as the refusal the
/// doc promised.
///
/// The case between the two suites is why the claim survived:
/// `marking_a_row_that_is_not_in_the_journal_is_refused` covers the absent row and
/// `sqlite_repricing_journal_store::a_decided_row_never_moves_again` covers the
/// trigger with raw SQL, so nothing drove `mark_applied` at a decided row and
/// looked at what class of error came back.
///
/// No money moved wrongly either way — the trigger still prevents the double
/// apply, which is `pending_for_run`'s "whole safety" — but the loser of a race
/// between two apply passes, or an apply pass racing `finish_run`'s straggler
/// sweep, was told the store had failed instead of being told another pass took
/// the row.
#[tokio::test]
async fn marking_a_row_a_concurrent_pass_already_decided_is_a_contention_not_a_store_failure() {
    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = a_run(&p, BulkKind::Repricing, "run-decided").await;
    let price_id = a_price_row(&p, "eu").await;
    let successor = a_price_row(&p, "successor").await;
    let other_successor = a_price_row(&p, "other-successor").await;

    repricing_journal_repo::open_rows(
        &conn,
        &scope(),
        &[NewJournalRow {
            run_id: run,
            price_id,
            tenant_id: TENANT,
        }],
    )
    .await
    .expect("freeze the row set");

    repricing_journal_repo::mark_applied(&conn, &scope(), TENANT, run, price_id, successor, at(11))
        .await
        .expect("the first pass takes the row");

    // A second apply pass reaches the same row. It is `applied`, not `pending`.
    let second = repricing_journal_repo::mark_applied(
        &conn,
        &scope(),
        TENANT,
        run,
        price_id,
        other_successor,
        at(12),
    )
    .await;
    let Err(RepoError::ConcurrentMutation { aggregate }) = second else {
        panic!(
            "a row another pass decided is a contention the caller can act on, not \
             a store failure; got {second:?}"
        );
    };
    assert!(
        aggregate.contains("applied"),
        "and the refusal names the state the row is in *now*, which is the fact \
         the caller does not already have: {aggregate}"
    );

    // The same for the failure swap, and against a row decided the other way — so
    // this is a property of the state conjunct and not of one terminal token.
    let second = repricing_journal_repo::mark_failed(
        &conn,
        &scope(),
        TENANT,
        run,
        price_id,
        "a straggler sweep arriving late",
    )
    .await;
    assert!(
        matches!(second, Err(RepoError::ConcurrentMutation { .. })),
        "same refusal for the failure path; got {second:?}"
    );

    // And the decision the first pass took is untouched by either refusal.
    let journal = repricing_journal_repo::list_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read the journal");
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].state, JournalState::Applied);
    assert_eq!(
        journal[0].applied_price_id,
        Some(successor),
        "the first pass's successor, not the second's"
    );
    assert_eq!(journal[0].applied_at, Some(at(11)));
    assert_eq!(journal[0].failure_reason, None);
}

#[tokio::test]
async fn list_for_run_and_pending_for_run_return_the_whole_run_not_a_page() {
    // Characterisation test, not a regression test: it began life as a falsification
    // probe against a brief that (wrongly) described `list_for_run` as page-bounded —
    // reading the function shows a bare `.all(runner)` with no `.limit(...)` anywhere,
    // and this test is that reading turned into an assertion. It is green the moment
    // it is written; that is what a characterisation test is. Its job is not to catch
    // today's bug — there is not one — but to catch tomorrow's: if a `.limit()` is ever
    // added to either query as an "optimisation" for a large run, this reddens instead
    // of the apply lane silently applying a prefix and marking the run complete.
    //
    // Chosen to exceed the crate's own REST hard cap of 1,000 (D-125: server default
    // 100, hard cap 1,000 — `cursor.rs:1`, `prices.rs:471`, `approvals.rs:700`,
    // `overlays.rs:192`), not its default of 100. The hard cap is the number the most
    // likely future "optimisation" would reach for — `.limit(1000)`, mirroring the
    // established convention — so a count that only cleared the default would stay
    // green while that exact regression truncated a large run.
    const ROW_COUNT: usize = 1_200;

    let p = provider().await;
    let conn = p.conn().expect("conn");
    let run = a_run(&p, BulkKind::Repricing, "run-characterisation").await;
    let mut price_ids = Vec::with_capacity(ROW_COUNT);
    for i in 0..ROW_COUNT {
        price_ids.push(a_price_row(&p, &format!("region-{i}")).await);
    }
    let new_rows: Vec<NewJournalRow> = price_ids
        .iter()
        .map(|&price_id| NewJournalRow {
            run_id: run,
            price_id,
            tenant_id: TENANT,
        })
        .collect();
    repricing_journal_repo::open_rows(&conn, &scope(), &new_rows)
        .await
        .expect("freeze the whole row set");

    let whole = repricing_journal_repo::list_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read the journal");
    assert_eq!(
        whole.len(),
        ROW_COUNT,
        "list_for_run must return every row of the run, not a page of it"
    );
    let mut expected_ids = price_ids.clone();
    expected_ids.sort_unstable();
    let mut read_ids: Vec<_> = whole.iter().map(|row| row.price_id).collect();
    read_ids.sort_unstable();
    assert_eq!(read_ids, expected_ids);
    assert!(whole.iter().all(|row| row.state == JournalState::Pending));

    let pending = repricing_journal_repo::pending_for_run(&conn, &scope(), TENANT, run)
        .await
        .expect("read what is left");
    assert_eq!(
        pending.len(),
        ROW_COUNT,
        "pending_for_run must see every pending row of the run, not a page of it"
    );
    let mut pending_ids: Vec<_> = pending.iter().map(|row| row.price_id).collect();
    pending_ids.sort_unstable();
    assert_eq!(pending_ids, expected_ids);
}
