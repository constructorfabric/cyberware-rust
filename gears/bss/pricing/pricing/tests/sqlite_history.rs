//! The price-history read (`inst-he-read`) against the executed `SQLite` mirror.
//!
//! Every claim this suite makes is a claim about what a **query** returns, which
//! is why none of it is a unit test. The walk's order is an `ORDER BY` over two
//! columns and not an in-memory sort of rows this file happened to hand over;
//! the cursor's resume is a `WHERE` the database evaluates; the actor is a column
//! read out of `pricing_price`; and the effective dating is a second table joined
//! by id. A page assembled from a vector would assert that the assembly compiles.
//!
//! # The fixture's two orders disagree on purpose
//!
//! The four seeded rows are laid out so that `price_id` order and authoring order
//! are **completely different** — the earliest row carries the largest id and the
//! latest carries the smallest. That is the whole content of "chronological":
//! `PriceRepo::list_for_plan_page` already walks `price_id ASC`, and a history
//! read that inherited it would be green against a fixture whose two orders
//! happened to agree, while returning a price history in an order nobody
//! authored anything in.
//!
//! # Two rows share one instant, and that is not an edge case
//!
//! `created_at_utc` is the request's instant, and a publish unit, a clone and a
//! repricing run each write several rows at one of them. So a tie is the normal
//! case rather than a curiosity, and it is what decides whether the keyset
//! predicate is written correctly: a walk resuming on the instant alone skips the
//! rest of a tied group, and one resuming on `>=` returns the cursor's own row
//! again on every page. The suite walks the whole history one row at a time,
//! which is the only page size at which both mistakes are visible.
//!
//! # Why one row is authored and three are seeded
//!
//! `PriceRepo::create_draft` writes an audit record; a direct `price::Entity`
//! insert writes none. Mixing them is what makes the actor case say something:
//! D-12's 2026-07-28 amendment puts the history record's actor on the row's own
//! `created_by` and forbids `pricing_audit_log`, and a fixture in which every row
//! had an audit record would be equally green either way, because the ordinary
//! authoring path stamps the same principal in both places. Here three of the
//! four rows have no audit record at all, so a read that consulted that table
//! would lose them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::history::{
    HistoryEntry, HistoryExporter, HistoryPage, HistoryPageRequest, encode,
};
use bss_pricing::infra::storage::entity::{audit_log, price};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::price_repo::HistoryPosition;
use bss_pricing::infra::storage::repo::window_repo::{self, NewWindow};
use bss_pricing::infra::storage::repo::{NewPriceDraft, PriceRepo};
use time::OffsetDateTime;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const PLAN: Uuid = Uuid::from_u128(0x91_a1);
const PHASE: Uuid = Uuid::from_u128(0x40_a5);

/// The earliest row, carrying the **largest** id — see the module doc.
const EARLY: Uuid = Uuid::from_u128(0xff_01);
/// Two rows authored at one instant, in id order.
const TIED_FIRST: Uuid = Uuid::from_u128(0x00_a1);
const TIED_SECOND: Uuid = Uuid::from_u128(0x00_a2);
/// The latest row, carrying the **smallest** id.
const LATE: Uuid = Uuid::from_u128(0x00_02);

/// The four rows in the order the history read must produce them.
const CHRONOLOGICAL: [Uuid; 4] = [EARLY, TIED_FIRST, TIED_SECOND, LATE];

/// Whoever authored the three seeded rows.
const SEEDING_ACTOR: Uuid = Uuid::from_u128(0xac_01);
/// Whoever authored the one row that went through the authoring door — a
/// **different** principal, so an actor read from the wrong row is visible.
const AUTHORING_ACTOR: Uuid = Uuid::from_u128(0xac_02);

/// One value for the whole binary: this suite drives a repository directly,
/// where the value the HTTP edge would have established has no producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// `2026-08-02T<hour>:00:00Z` — the authoring scale.
fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 2, hour, 0, 0)
}

/// `2099-09-<day>T00:00:00Z` — the effective-dating scale.
///
/// A different year from the authoring one, and far enough ahead that "future"
/// is a fact rather than a fixture that ages: `sqlite_window_repo.rs` records
/// what happens when a window suite's instants drift into the past.
fn effective(day: u32) -> OffsetDateTime {
    utc_ymd_hms(2099, 9, day, 0, 0, 0)
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: AUTHORING_ACTOR,
        recorded_at: at(10),
        correlation_id: TEST_CORRELATION,
    }
}

/// A key differing from its siblings in exactly one axis, the currency.
///
/// Distinct keys because the two partial `UNIQUE` indexes over `pricing_price`
/// admit one draft and one published row per canonical key, and this fixture
/// wants four rows that coexist rather than four that supersede one another.
fn key(currency: &str) -> ScopeKey {
    ScopeKey::new(
        PlanId::new(PLAN),
        CurrencyCode::new(currency).expect("three letters"),
        Region::new("EU").expect("a non-blank region"),
        PhaseId::new(PHASE),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

async fn harness() -> (HistoryExporter, DBProvider<DbError>) {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    seed(&provider).await;
    (HistoryExporter::new(provider.clone()), provider)
}

/// The four rows, plus one belonging to another tenant.
///
/// The lifecycle states are deliberately three different ones. History is the
/// append-only row sequence and not the authoring surface's working set, so a
/// read that inherited `list_for_plan_page`'s state filter would drop the
/// `superseded` predecessor — the one row an operator asking "what did this
/// price used to say" is most likely asking about.
async fn seed(provider: &DBProvider<DbError>) {
    seed_row(provider, TENANT, EARLY, "USD", "superseded", at(9)).await;
    seed_row(provider, TENANT, TIED_FIRST, "EUR", "published", at(10)).await;
    seed_row(provider, TENANT, TIED_SECOND, "GBP", "draft", at(10)).await;

    // The one row that goes through the authoring door, so the projection is
    // exercised against a row the system can really produce - and so exactly one
    // audit record exists in the whole fixture.
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_000).expect("a non-negative amount"));
    PriceRepo::new(provider.clone())
        .create_draft(
            &scope(),
            TENANT,
            NewPriceDraft {
                price_id: LATE,
                scope_key: key("JPY"),
                content: PriceContent {
                    row,
                    tax_inclusive: false,
                    tax_category_ref: None,
                    billing_timing: Some("advance".to_owned()),
                    proration_contract: None,
                    rounding_policy_ref: None,
                    grandfather_until: None,
                    supersedes_price_id: None,
                },
                created_by: AUTHORING_ACTOR,
                created_at_utc: at(11),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the last row");

    // A neighbour's row at an instant right in the middle of ours.
    seed_row(
        provider,
        OTHER_TENANT,
        Uuid::from_u128(0xbb_01),
        "USD",
        "draft",
        at(10),
    )
    .await;
}

/// One price row, written through its entity under the tenant that owns it and
/// **without** an audit record.
async fn seed_row(
    provider: &DBProvider<DbError>,
    tenant_id: Uuid,
    price_id: Uuid,
    currency: &str,
    lifecycle_state: &str,
    authored_at: OffsetDateTime,
) {
    let conn = provider.conn().expect("scoped connection");
    let row = price::ActiveModel {
        price_id: Set(price_id),
        tenant_id: Set(tenant_id),
        plan_id: Set(PLAN),
        currency: Set(currency.to_owned()),
        region: Set("EU".to_owned()),
        phase: Set(PHASE),
        charge_kind: Set("recurring".to_owned()),
        amount_minor: Set(Some(1_000)),
        model_kind: Set(Some("flat".to_owned())),
        lifecycle_state: Set(lifecycle_state.to_owned()),
        created_by: Set(SEEDING_ACTOR),
        created_at_utc: Set(authored_at),
        ..price::ActiveModel::default()
    };
    price::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::for_tenant(tenant_id), &row)
        .expect("scope the seeded price row")
        .exec(&conn)
        .await
        .unwrap_or_else(|e| panic!("seed price row {price_id}: {e}"));
}

/// The ids a page carries, in the order it carries them.
fn ids(page: &HistoryPage) -> Vec<Uuid> {
    page.entries
        .iter()
        .map(|entry| entry.record.price_id)
        .collect()
}

async fn read(
    exporter: &HistoryExporter,
    limit: u64,
    after: Option<HistoryPosition>,
) -> HistoryPage {
    exporter
        .read_page(
            &scope(),
            TENANT,
            HistoryPageRequest::resuming(Some(limit), after).expect("a page of at least one row"),
        )
        .await
        .expect("the history read must succeed")
}

// ---------------------------------------------------------------------------
// The order, and what it is not

#[tokio::test]
async fn the_page_is_chronological_and_not_in_key_order() {
    let (exporter, _provider) = harness().await;

    let page = read(&exporter, 100, None).await;

    assert_eq!(
        ids(&page),
        CHRONOLOGICAL,
        "history is commit-ordered (D-125)"
    );
    // Said out loud so the case cannot be read as coincidence: sorting the same
    // four rows by id gives a different sequence, and it is the sequence
    // `list_for_plan_page` would have produced.
    let mut by_id = CHRONOLOGICAL;
    by_id.sort();
    assert_ne!(by_id, CHRONOLOGICAL);
}

#[tokio::test]
async fn history_spans_every_lifecycle_state() {
    let (exporter, _provider) = harness().await;

    let page = read(&exporter, 100, None).await;

    let states: Vec<LifecycleState> = page
        .entries
        .iter()
        .map(|entry| entry.record.lifecycle_state)
        .collect();
    assert_eq!(
        states,
        [
            LifecycleState::Superseded,
            LifecycleState::Published,
            LifecycleState::Draft,
            LifecycleState::Draft,
        ]
    );
}

#[tokio::test]
async fn a_foreign_tenants_rows_are_not_in_the_page() {
    let (exporter, _provider) = harness().await;

    let page = read(&exporter, 100, None).await;

    assert_eq!(page.entries.len(), CHRONOLOGICAL.len());
    // The neighbour's row sits at the fixture's busiest instant, so a walk that
    // leaked it would leak it into the middle of the page rather than at an end.
    assert!(!ids(&page).contains(&Uuid::from_u128(0xbb_01)));
}

// ---------------------------------------------------------------------------
// The cursor

#[tokio::test]
async fn a_one_row_walk_visits_every_row_exactly_once_across_a_tie() {
    let (exporter, _provider) = harness().await;

    let mut seen: Vec<Uuid> = Vec::new();
    let mut after: Option<HistoryPosition> = None;
    // Bounded so a walk that never advances fails as an assertion rather than as
    // a hung test.
    for _ in 0..CHRONOLOGICAL.len() {
        let page = read(&exporter, 1, after).await;
        seen.extend(ids(&page));
        after = page.next;
    }

    assert_eq!(seen, CHRONOLOGICAL, "no row skipped and none served twice");
    assert_eq!(after, None, "the walk is exhausted, and says so");
}

#[tokio::test]
async fn the_last_page_carries_no_cursor_and_never_exceeds_the_limit() {
    let (exporter, _provider) = harness().await;

    let full = read(&exporter, 100, None).await;
    assert_eq!(full.next, None, "a client must be able to stop here");

    // The probe row the read asks for must not reach the caller.
    let bounded = read(&exporter, 2, None).await;
    assert_eq!(bounded.entries.len(), 2);
    assert!(bounded.next.is_some());
}

#[tokio::test]
async fn a_cursor_serialized_and_read_back_resumes_the_same_walk() {
    let (exporter, _provider) = harness().await;

    let first = read(&exporter, 1, None).await;
    let token = encode(first.next.expect("three rows remain"));
    let resumed = HistoryPageRequest::parse(Some(100), Some(&token)).expect("our own token");

    let page = read(&exporter, resumed.limit(), resumed.after().copied()).await;

    assert_eq!(ids(&page), CHRONOLOGICAL[1..]);
}

// ---------------------------------------------------------------------------
// The actor (D-12)

#[tokio::test]
async fn the_actor_is_the_rows_own_column_and_not_the_audit_log() {
    let (exporter, provider) = harness().await;
    let conn = provider.conn().expect("conn");

    // Exactly one audit record exists in the whole fixture, and it belongs to
    // the one row that went through the authoring door. Three of the four rows
    // are therefore invisible to `pricing_audit_log` entirely.
    let audited = audit_log::Entity::find()
        .secure()
        .scope_with(&scope())
        .filter(Condition::all().add(audit_log::Column::TenantId.eq(TENANT)))
        .count(&conn)
        .await
        .expect("count the audit records");
    assert_eq!(audited, 1);

    let page = read(&exporter, 100, None).await;

    let actors: Vec<Uuid> = page.entries.iter().map(HistoryEntry::actor).collect();
    assert_eq!(
        actors,
        [SEEDING_ACTOR, SEEDING_ACTOR, SEEDING_ACTOR, AUTHORING_ACTOR],
        "each record reports the principal on its own `created_by`"
    );
}

// ---------------------------------------------------------------------------
// The effective dates

#[tokio::test]
async fn an_entry_carries_every_interval_its_row_is_effective_over() {
    let (exporter, provider) = harness().await;
    let conn = provider.conn().expect("conn");

    // Two adjacent intervals on one row, scheduled out of chronological order so
    // that the `from` ordering is the store's answer and not the insertion's.
    // Adjacency is legal and is the shape a supersession leaves: the interval is
    // half-open, so `effective_to == next.effective_from` is not an overlap.
    for (id, from, to) in [
        (0xf0_01_u128, effective(10), effective(20)),
        (0xf0_02_u128, effective(1), effective(10)),
    ] {
        window_repo::schedule(
            &conn,
            &scope(),
            NewWindow {
                window_id: Uuid::from_u128(id),
                tenant_id: TENANT,
                price_id: EARLY,
                effective_from: from,
                effective_to: Some(to),
                reason_code: "historyFixture".to_owned(),
            },
            stamp(),
        )
        .await
        .unwrap_or_else(|e| panic!("schedule the fixture window: {e}"));
    }

    let page = read(&exporter, 100, None).await;

    let early = page
        .entries
        .iter()
        .find(|entry| entry.record.price_id == EARLY)
        .expect("the earliest row is in the page");
    assert_eq!(
        early
            .effective
            .iter()
            .map(|interval| (interval.from, interval.to, interval.state))
            .collect::<Vec<_>>(),
        [
            (effective(1), Some(effective(10)), WindowState::Scheduled),
            (effective(10), Some(effective(20)), WindowState::Scheduled),
        ],
        "both intervals, oldest first"
    );

    // A row nobody has scheduled coverage for carries none - a draft, or a
    // clone whose windows the operator has not authored yet (`inst-cl-windows`).
    let unscheduled = page
        .entries
        .iter()
        .find(|entry| entry.record.price_id == LATE)
        .expect("the authored row is in the page");
    assert!(unscheduled.effective.is_empty());
}
