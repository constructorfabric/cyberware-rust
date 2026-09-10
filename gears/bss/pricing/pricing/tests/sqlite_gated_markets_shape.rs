//! The **shape** of the read `price_repo::gated_markets` issues — that it is
//! bounded, and that bounding it did not change the number it answers.
//!
//! This was the one read on any of the three tickers with no `.limit()` at all: a
//! `price::Entity::find()… .all(runner)` over every published, non-grandfathered,
//! tax-inclusive row in the catalog, every 60 seconds, deduplicated into markets
//! afterwards. Its stated bound was *"the cost is bounded by the thing being
//! measured — the gated rows **are** the backlog"*, and that is not a bound: the
//! backlog is every tax-inclusive row every tenant has published, which the PRD's
//! own risk table sizes at *"an estimated eight months"*.
//!
//! # What is asserted, and why it is the statement text
//!
//! The claim is *"no unbounded read on a ticker"*, and the only observable that
//! distinguishes a bounded read from an unbounded one is the statement itself: a
//! paged shape and an unpaged one can return the same rows and the same count.
//! `sqlx` logs every statement on the `sqlx::query` target, so the probe reads the
//! `SELECT`s this call issues against `pricing_price` and requires each to carry a
//! `LIMIT`. A statement count would be the wrong measure here and would move the
//! wrong way — paging *adds* statements — which is why this file counts them only
//! to prove the page actually broke.
//!
//! # Why the page size is a parameter
//!
//! Proving the pages compose needs a seed that spans more than one page, and the
//! production page is 1000 rows — a seed nobody wants to write through
//! `create_draft`. So the walk takes its page size as an argument
//! ([`price_repo::gated_markets_in_pages`]) and the production entry point spends
//! the constant, which is the arrangement `gated_markets`' own `tax_engine_ga`
//! parameter already uses for the same reason: a value reasoned about in prose and
//! reachable from no test is a value the next reader has to re-derive.
//!
//! One test, one process, one counter — `tests/sqlite_overlay_world_fanout.rs`
//! states why a global subscriber cannot share a binary, and this file inherits it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::storage::entity::price;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::price_repo;
use bss_pricing::infra::storage::repo::{NewPriceDraft, PriceRepo};

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

/// Published tax-inclusive rows the seed writes, over [`MARKETS`] distinct
/// markets. Both are read back into the assertions.
const ROWS: usize = 5;

/// Distinct `(tenant, currency, region)` markets among those rows — two rows share
/// one region, so a count that forgot to deduplicate reads 5 rather than 4.
const MARKETS: i64 = 4;

/// A page small enough that [`ROWS`] rows span three of them, and small enough
/// that one page break falls **inside** the two rows sharing a market.
const PAGE: u64 = 2;

/// Counts the `SELECT`s `sqlx` logs against `pricing_price`, and how many of them
/// carried a `LIMIT`.
struct StatementCounter {
    selects: Arc<AtomicUsize>,
    limited: Arc<AtomicUsize>,
}

impl Subscriber for StatementCounter {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "sqlx::query"
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut sql = StatementText(String::new());
        event.record(&mut sql);
        // `pricing_price_tier_band` and `pricing_price_window` both contain
        // `pricing_price`, so the table is matched with its closing quote — the
        // reads this call makes are of the row table alone, and a band read
        // counted here would be a statement this claim says nothing about.
        if sql.0.contains("\"pricing_price\"") && sql.0.contains("SELECT") {
            self.selects.fetch_add(1, Ordering::Relaxed);
            if sql.0.contains("LIMIT") {
                self.limited.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Every field of the event, concatenated — see
/// `tests/sqlite_overlay_world_fanout.rs` for why all of them are read.
struct StatementText(String);

impl Visit for StatementText {
    fn record_str(&mut self, _: &Field, value: &str) {
        self.0.push_str(value);
    }

    #[allow(clippy::use_debug, clippy::let_underscore_must_use)]
    fn record_debug(&mut self, _: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{value:?}");
    }
}

fn tenant() -> Uuid {
    Uuid::from_u128(0x7e_11)
}

fn plan() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
}

/// A key on the named market. `eligibility` is what lets **two** keys share one
/// market: `all_subscriptions` and `new_subscriptions_only` are different scope
/// keys — `DuplicateScopeKey` refuses a second row on one — and the same
/// `(tenant, currency, region)`, which is what `gated_markets` deduplicates on.
fn market_key(region: &str, eligibility: PriceEligibility) -> ScopeKey {
    ScopeKey::new(
        plan(),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new(region).expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0xfa_5e)),
        eligibility,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("both eligibilities pair with cohort none")
}

/// A flat recurring row, tax-inclusive — the predicate `gated_markets` counts.
fn tax_inclusive_flat() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(1_000).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: true,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

#[tokio::test]
async fn the_gated_market_read_is_paged_and_still_counts_every_market_once() {
    let selects = Arc::new(AtomicUsize::new(0));
    let limited = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(StatementCounter {
        selects: Arc::clone(&selects),
        limited: Arc::clone(&limited),
    })
    .expect("this binary installs the only subscriber");

    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    let repo = PriceRepo::new(provider.clone());
    let scope = AccessScope::for_tenant(tenant());
    let all = AccessScope::allow_all();

    // Five published tax-inclusive rows over four markets: `EU` twice, so the
    // deduplication is load-bearing, and the two `EU` rows are **not adjacent** so
    // that one market straddles a page boundary.
    //
    // They were adjacent (`0xda_01`/`0xda_02`) until 2026-08-20, and adjacency is
    // the opposite of what this needs: the walk pages by `price_id ASC`, so
    // adjacent rows of one market land on one page and every page then holds whole
    // market groups. Measured — the pages were `[{EU}, {APAC, US}, {LATAM}]`, so a
    // per-page dedup that summed the page counts answered 1 + 2 + 1 = 4, exactly
    // `MARKETS`, and passed. The `US` row is interleaved between them instead, and
    // the assertion below is what keeps the arrangement true rather than merely
    // described.
    for (n, region, eligibility) in [
        (0xda_01_u128, "EU", PriceEligibility::AllSubscriptions),
        (0xda_02, "US", PriceEligibility::AllSubscriptions),
        (0xda_03, "EU", PriceEligibility::NewSubscriptionsOnly),
        (0xda_04, "APAC", PriceEligibility::AllSubscriptions),
        (0xda_05, "LATAM", PriceEligibility::AllSubscriptions),
    ] {
        let price_id = Uuid::from_u128(n);
        repo.create_draft(
            &scope,
            tenant(),
            NewPriceDraft {
                price_id,
                scope_key: market_key(region, eligibility),
                content: tax_inclusive_flat(),
                created_by: Uuid::from_u128(0xac_10),
                created_at_utc: utc_ymd_hms(2026, 8, 2, 10, 0, 0),
                correlation_id: Uuid::from_u128(0xc0_11),
            },
        )
        .await
        .expect("create the draft");
        common::publish_row_directly(&provider, &scope, price_id).await;
    }

    let conn = provider.conn().expect("conn");

    // **The arrangement, asserted rather than described.** Which rows share a page
    // is a fact about the seeded ids, and the seed above says why it matters: if
    // every page held whole market groups, a per-page dedup summing its pages
    // would answer `MARKETS` and this suite could not tell it from the global one.
    let ordered = price::Entity::find()
        .secure()
        .scope_with(&all)
        .filter(
            Condition::all()
                .add(price::Column::LifecycleState.eq("published"))
                .add(price::Column::TaxInclusive.eq(true)),
        )
        .order_by(price::Column::PriceId, Order::Asc)
        .all(&conn)
        .await
        .expect("read the rows the walk will page over");
    assert_eq!(
        ordered.len(),
        ROWS,
        "every seeded row is one the walk will see"
    );
    let per_page: Vec<BTreeSet<(String, String)>> = ordered
        .chunks(usize::try_from(PAGE).expect("a small page"))
        .map(|page| {
            page.iter()
                .map(|row| (row.currency.clone(), row.region.clone()))
                .collect()
        })
        .collect();
    let summed: usize = per_page.iter().map(BTreeSet::len).sum();
    assert!(
        summed > usize::try_from(MARKETS).expect("a small count"),
        "the shared market must straddle a page boundary, or a per-page dedup would sum to \
         MARKETS ({MARKETS}) and pass: {per_page:?}"
    );

    selects.store(0, Ordering::Relaxed);
    limited.store(0, Ordering::Relaxed);
    let counted = price_repo::gated_markets_in_pages(&conn, &all, false, PAGE)
        .await
        .expect("the gated read succeeds");
    let issued = selects.load(Ordering::Relaxed);
    let bounded = limited.load(Ordering::Relaxed);

    // The counter is armed. Without this, a mechanism observing nothing would
    // satisfy "every statement carried a LIMIT" vacuously — zero of zero.
    assert!(
        issued > 0,
        "the statement counter observed no read at all, so it is measuring nothing"
    );
    // The answer, before the shape: a paging change that dropped rows would
    // otherwise satisfy the bound assertion by reading less.
    assert_eq!(
        counted, MARKETS,
        "{ROWS} published tax-inclusive rows over {MARKETS} markets, deduplicated"
    );
    // The claim itself. Not a statement count — paging *adds* statements — but
    // that no statement this ticker issues is unbounded.
    assert_eq!(
        bounded,
        issued,
        "every read on the gated-market path must carry a LIMIT; {} of {issued} did not",
        issued - bounded
    );
    // And the page really broke, so the assertion above is not describing a
    // single-page read that would have been bounded either way.
    assert!(
        issued > 1,
        "a {PAGE}-row page over {ROWS} rows must take more than one statement, or the \
         multi-page composition below is untested"
    );

    // The production entry point answers the same number over the same catalog —
    // the page size is the only difference between them, and a caller that spent
    // the constant on a catalog this small would read one page.
    assert_eq!(
        price_repo::gated_markets(&conn, &all, false)
            .await
            .expect("the gated read succeeds"),
        MARKETS,
        "the constant-page entry point and the parameterized walk are one read"
    );

    // The GA short-circuit is upstream of the walk and stays there: on the day the
    // constant flips this must answer 0 rather than the backlog.
    assert_eq!(
        price_repo::gated_markets_in_pages(&conn, &all, true, PAGE)
            .await
            .expect("the gated read succeeds"),
        0,
        "past GA the backlog is empty by definition, not by paging"
    );
}
