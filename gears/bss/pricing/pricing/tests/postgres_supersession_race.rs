//! The supersession unit's serialization point, on real Postgres.
//!
//! # Why this cannot be a `SQLite` suite, and why it is owed
//!
//! `sqlite::memory:` **serializes writers**, so the interleaving under test can be
//! neither confirmed nor refuted there. `tests/sqlite_supersession.rs` proves that the
//! four writes land in one order and roll back together; what it cannot show is what
//! happens when two commits on one key are **in flight at the same time**.
//!
//! It is owed because `infra::supersession` chose a write order between the two planes on
//! a stated ground that a 2026-08-05 review showed to be false. The review also pointed
//! out that the unit has a **serialization point** nobody had recorded — and that it is a
//! better basis for choosing the order than anything written down.
//! `window_repo::refuse_overlap` documents at length that it has none; this suite shows
//! that the *commit* does, on the predecessor **price row**, one statement in.
//!
//! # The choreography, and why each step is there
//!
//! A concurrency test that starts two tasks and asserts on the outcome is a coin toss
//! with a green side. This one is driven by observable events only, in the idiom
//! `tests/postgres_approval_race.rs` established:
//!
//! 1. the first commit runs all four of its writes and then **parks**, holding the row
//!    lock on the predecessor's price row;
//! 2. the second commit starts; its reads see the still-committed `published` row, and
//!    its `UPDATE` blocks on that lock;
//! 3. a third connection **observes the block** in `pg_locks`, which is what proves the
//!    second commit's read already happened;
//! 4. only then is the first released to commit, and the second's `UPDATE` re-evaluates
//!    its `lifecycle_state = 'published'` predicate under READ COMMITTED, matches
//!    nothing, and resolves into the refusal.
//!
//! Step 3 is the load-bearing one. Without it the second commit could read the
//! *already-superseded* row and be refused by its own precondition before ever
//! contending — green, and about nothing.
//!
//! # What this proves about the ordering, precisely
//!
//! The two commits are a **replay of one unit** — same ids, same presented act counter —
//! which is the realistic race here (a retry after a timeout, a double approval) rather
//! than two operators inventing the same changeover.
//!
//! **This suite is what decided the write order between the two planes.** Run against
//! windows-first, the loser blocks on the predecessor *window* and is answered
//! `StaleRowVersion`, which sends an operator to re-read a window entity tag that is not
//! what changed. Run against rows-first, it blocks on the predecessor *price row*, finds
//! it `superseded`, and is answered `NotSupersedable`, whose message names the actionable
//! remedy. The order is free for correctness, so diagnosis is what is left to choose on —
//! and this case is the measurement, not a restatement of it. Reorder the two pairs in
//! `commit_supersession` and this assertion flips.
//!
//! Ignored by default; it needs Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_supersession_race -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use std::sync::Arc;
use std::time::Duration;

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::supersession::{ChangeoverMoment, NamedWindow, plan_supersession};
use bss_pricing::domain::window::{WindowInterval, WindowState};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::repo::{NewPriceDraft, NewWindow, price_repo, window_repo};
use bss_pricing::infra::supersession::{SupersessionCommit, commit_supersession};
use time::OffsetDateTime;

use pg_support::Pg;
use tokio::sync::Notify;
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

/// The readiness a fixture publishes against.
///
/// **Empty deliberately, and safe for one reason only**: every row this suite
/// publishes states its own `tax_category_ref`, so nothing is ever resolved from
/// the region lookup.
///
/// It is *not* safe because a NULL `resolved_tax_category` is a shape a published
/// row may carry. That claim is measurably false and `rest_support/mod.rs` records
/// its withdrawal: `TaxBasisComplete` refuses such a row, `pricing_price`'s
/// migration header calls a NULL on a published row impossible, and
/// `trg_pricing_price_append_only` makes it unrepairable. Repeating it here is how
/// the fault the shared harness was repaired for comes back.
fn fixture_readiness() -> bss_pricing::domain::tax_display::RegionTaxReadiness {
    bss_pricing::domain::tax_display::RegionTaxReadiness::empty()
}

const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_30);
const TENANT: Uuid = Uuid::from_u128(0x7e_41);
const PREDECESSOR: Uuid = Uuid::from_u128(0xb_8001);
const SUCCESSOR: Uuid = Uuid::from_u128(0xb_8002);
const PREDECESSOR_WINDOW: Uuid = Uuid::from_u128(0xb_8f01);
const SUCCESSOR_WINDOW: Uuid = Uuid::from_u128(0xb_8f02);

/// The same bound `postgres_approval_race.rs` uses: long enough that a slow machine is
/// not a failure, short enough that a genuine deadlock is not a hung suite.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_3c6))
}

fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 1, 1, 0, 0, 0)
}

fn coverage_from() -> OffsetDateTime {
    utc_ymd_hms(2098, 6, 1, 0, 0, 0)
}

fn changeover() -> OffsetDateTime {
    utc_ymd_hms(2099, 4, 1, 0, 0, 0)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: Uuid::from_u128(0xac_30),
        recorded_at: now(),
        correlation_id: TEST_CORRELATION,
    }
}

fn key() -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_7e)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

fn content(amount: i64) -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(amount).expect("non-negative"));
    PriceContent {
        row,
        tax_inclusive: false,
        // Its own category, for the reason its own rounding policy is stated two
        // lines down: `publish_rows` refuses a publish that resolves no tax
        // category either (H14, review 2026-08-19), and this suite publishes
        // against `RegionTaxReadiness::empty()`.
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        // Its own policy, so the row resolves a rounding mode without a tenant
        // default: `publish_rows` refuses a set that resolves none at all (review
        // F1, 2026-08-19), and this suite's subject is the supersession race.
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn draft(price_id: Uuid, amount: i64) -> NewPriceDraft {
    NewPriceDraft {
        price_id,
        scope_key: key(),
        content: content(amount),
        created_by: Uuid::from_u128(0xac_30),
        created_at_utc: now(),
        correlation_id: TEST_CORRELATION,
    }
}

/// The commit both racers present: one composed plan, one act counter.
fn commit_plan(shorten_seq: u64) -> SupersessionCommit {
    let plane = [NamedWindow {
        window_id: PREDECESSOR_WINDOW,
        interval: WindowInterval::new(coverage_from(), None, WindowState::Scheduled),
    }];
    let plan = plan_supersession(
        &content(1_000).row,
        &content(1_200).row,
        &plane,
        changeover(),
        now(),
        ChangeoverMoment::Commit,
    )
    .expect("the fixture world composes");
    SupersessionCommit::of_plan(
        &plan,
        plan_id(),
        PREDECESSOR,
        shorten_seq,
        (SUCCESSOR, RowVersion::new(0)),
        SUCCESSOR_WINDOW,
        "repricing".to_owned(),
    )
}

/// A published predecessor with open-ended coverage and the successor draft staged
/// beside it — the world a composed unit commits against.
///
/// The successor goes through `insert_successor_draft_on`, the real door, for
/// `tests/sqlite_supersession.rs`'s reason: a commit staged by anything else would be a
/// commit over a world the gear cannot produce. The predecessor's publish is fabricated
/// with a direct `UPDATE`, which is that suite's `flip_state` and its reason too.
async fn seed(pg: &Pg) -> u64 {
    let provider = toolkit_db::DBProvider::<toolkit_db::DbError>::new(pg.db().await);
    let repo = bss_pricing::infra::storage::repo::PriceRepo::new(provider.clone());
    repo.create_draft(&scope(), TENANT, draft(PREDECESSOR, 1_000))
        .await
        .expect("author the predecessor");

    // The predecessor's publish, through `publish_rows` rather than a raw UPDATE: this
    // suite has a real transaction to hand and the sibling `SQLite` suite's `flip_state`
    // shortcut exists only because a one-row suite should not need a publish unit. Here
    // the flip is one call and it exercises the sanctioned producer.
    let (_, published) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                price_repo::publish_rows(
                    txn,
                    &scope(),
                    TENANT,
                    plan_id(),
                    &[(PREDECESSOR, RowVersion::new(0))],
                    &fixture_readiness(),
                    None,
                )
                .await
                .map(|_| ())
            })
        })
        .await;
    published.expect("publish the predecessor");

    let window = window_repo::schedule(
        &provider.conn().expect("conn"),
        &scope(),
        NewWindow {
            window_id: PREDECESSOR_WINDOW,
            tenant_id: TENANT,
            price_id: PREDECESSOR,
            effective_from: coverage_from(),
            effective_to: None,
            reason_code: "initialCoverage".to_owned(),
        },
        stamp(),
    )
    .await
    .expect("the predecessor's open-ended coverage");

    let (_, outcome) = provider
        .db()
        .in_transaction::<(), RepoError, _>(move |txn| {
            Box::pin(async move {
                Box::pin(price_repo::insert_successor_draft_on(
                    txn,
                    &scope(),
                    TENANT,
                    draft(SUCCESSOR, 1_200),
                ))
                .await
                .map(|_| ())
            })
        })
        .await;
    outcome.expect("stage the successor through the door");

    window.mutation_seq
}

#[tokio::test]
#[ignore = "requires Postgres; run with --ignored"]
async fn two_commits_of_one_unit_serialize_on_the_predecessor_row_and_one_is_refused() {
    let pg = Pg::applied().await;
    let seq = seed(&pg).await;

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // The winner: all four writes, then park holding the row lock on the predecessor's
    // price row -- the rows are written first, so that is the lock the replay meets.
    let winner = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        let plan = commit_plan(seq);
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        commit_supersession(txn, &scope(), TENANT, plan, stamp()).await?;
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    // The replay: same ids, same presented act counter. Its `UPDATE` on the predecessor
    // price row blocks -- one statement into the commit, before either window is touched.
    let replay = {
        let db = pg.db().await;
        let plan = commit_plan(seq);
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        commit_supersession(txn, &scope(), TENANT, plan, stamp())
                            .await
                            .map(|_| ())
                    })
                })
                .await;
            out
        })
    };

    // The replay's reads have happened and its UPDATE is waiting on the lock. Only now is
    // the winner allowed to commit.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, winner)
        .await
        .expect("the winner must finish once released")
        .expect("its task must not panic")
        .expect("the winner is uncontended and must commit");

    let refusal = tokio::time::timeout(RACE_TIMEOUT, replay)
        .await
        .expect("the replay must be released by the winner's commit")
        .expect("its task must not panic")
        .expect_err("the act counter moved under it");

    // The row plane answers, and it answers with a precondition rather than a fault: the
    // unit's serialization point is the predecessor **price row**, one statement into the
    // commit.
    // `in_transaction` wraps the body's error, so the domain refusal is unwrapped before
    // it is judged — a `TxError::Infra` here would be the store failing rather than the
    // precondition answering, which is the distinction this assertion is about.
    let refusal = refusal.into_domain(|infra| RepoError::Db(format!("race transaction: {infra}")));

    // The **row** plane answers, because the rows are written first, and it answers with
    // the refusal whose message names what to do: recompose against the key's new current
    // row. Reversing the two pairs in `commit_supersession` turns this into the window
    // plane's `StaleRowVersion` — which is the measurement that chose the order.
    let RepoError::NotSupersedable { id, state, .. } = refusal else {
        panic!("the loser must get the actionable refusal, not a fault: {refusal:?}");
    };
    assert_eq!(id, PREDECESSOR.to_string());
    assert_eq!(state, LifecycleState::Superseded.as_str());

    // And the winner's four writes are all there, exactly once.
    let provider = toolkit_db::DBProvider::<toolkit_db::DbError>::new(pg.db().await);
    let repo = bss_pricing::infra::storage::repo::PriceRepo::new(provider.clone());
    assert_eq!(
        repo.find(&scope(), TENANT, PREDECESSOR)
            .await
            .expect("read")
            .expect("there")
            .lifecycle_state,
        LifecycleState::Superseded
    );
    assert_eq!(
        repo.find(&scope(), TENANT, SUCCESSOR)
            .await
            .expect("read")
            .expect("there")
            .lifecycle_state,
        LifecycleState::Published
    );

    let conn = provider.conn().expect("conn");
    let successor_window = window_repo::find(&conn, &scope(), TENANT, SUCCESSOR_WINDOW)
        .await
        .expect("read")
        .expect("the successor's window was scheduled");
    assert_eq!(successor_window.effective_from, changeover());
    assert_eq!(successor_window.effective_to, None);

    let shortened = window_repo::find(&conn, &scope(), TENANT, PREDECESSOR_WINDOW)
        .await
        .expect("read")
        .expect("there");
    assert_eq!(shortened.effective_to, Some(changeover()));
    assert_eq!(
        shortened.mutation_seq,
        seq + 1,
        "exactly one act touched the predecessor's window, so the replay wrote nothing"
    );
}
