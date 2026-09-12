//! Tests for `RateLocker`.
//!
//! The single-currency short-circuit (`transaction_ccy == functional_ccy` →
//! `Ok(None)`, functional columns left NULL) returns BEFORE any DB access, so it
//! runs against a bare in-memory SQLite `Db` with no migrations. The
//! cross-currency translate-path (resolve → snapshot insert → stamp) needs a
//! database for the snapshot insert, so it is a Docker-gated `#[ignore]` stub the
//! controller fills in alongside the live posting wiring.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use bss_ledger_sdk::{AccountClass, MappingStatus, Side};
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

use super::*;
use crate::config::FxConfig;
use crate::domain::model::NewLine;
use crate::infra::storage::repo::FxRepo;
use time::OffsetDateTime;

/// A minimal AR `NewLine` in `currency`, functional columns unset — the input
/// shape `lock_and_stamp` reads (`account_class`, `side`, `amount_minor`) and
/// writes (`functional_*`, `rate_snapshot_ref`).
fn ar_line(amount_minor: i64, side: Side, currency: &str) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: Uuid::now_v7(),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::now_v7(),
        account_class: AccountClass::Ar,
        gl_code: None,
        side,
        amount_minor,
        currency: currency.to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        legal_entity_id: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

/// Build a `RateLocker` over a bare in-memory SQLite provider (no migrations) —
/// enough for the single-currency short-circuit, which never touches the DB.
async fn locker_no_db() -> RateLocker {
    // A shared-cache in-memory SQLite DB; the single-currency path returns before
    // any query, so the (empty) schema is irrelevant.
    let db = connect_db(
        "sqlite:file:fx_rate_locker_unit?mode=memory&cache=shared",
        ConnectOpts::default(),
    )
    .await
    .unwrap();
    let provider = DBProvider::<DbError>::new(db);
    let repo = FxRepo::new(provider);
    let source = RateSource::new(repo.clone(), FxConfig::default());
    RateLocker::new(source, repo)
}

#[tokio::test]
async fn single_currency_returns_none_and_leaves_functional_null() {
    let locker = locker_no_db().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    // A balanced single-currency entry (USD == USD): no FX, no stamping.
    let mut lines = vec![
        ar_line(1000, Side::Debit, "USD"),
        ar_line(1000, Side::Credit, "USD"),
    ];

    let result = locker
        .lock_and_stamp(
            &scope,
            tenant,
            &mut lines,
            "USD",
            "USD",
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("single-currency lock must succeed");

    // No snapshot minted.
    assert_eq!(result, None, "single-currency must return Ok(None)");
    // Functional columns untouched on every line.
    for line in &lines {
        assert_eq!(line.functional_amount_minor, None);
        assert_eq!(line.functional_currency, None);
    }
}

/// Cross-currency translate-path (resolve → snapshot insert → stamp): needs a
/// migrated database for the `ledger_fx_rate` candidate + the
/// `ledger_fx_rate_snapshot` insert. Left as a Docker-gated stub the controller
/// completes when it wires the functional-currency source (design S5-F3) and the
/// live posting paths; the expected post-conditions are spelled out below.
#[tokio::test]
#[ignore = "requires Docker (testcontainers) — controller completes the cross-currency path"]
async fn cross_currency_stamps_functional_and_returns_snapshot_id() {
    // Expected, once a migrated Postgres + a seeded `ledger_fx_rate` row exist:
    //   1. `lock_and_stamp(scope, tenant, &mut lines, "EUR", "USD", now)` resolves
    //      the EUR->USD rate (the provider-ordered, non-stale candidate),
    //   2. inserts ONE `ledger_fx_rate_snapshot` row (returned `Some(rate_id)`),
    //   3. sets every line's `functional_amount_minor` (the translated USD amount,
    //      residual closed onto the AR anchor), `functional_currency == "USD"`,
    //      and `rate_snapshot_ref == rate_id`,
    //   4. the functional column nets to zero (DR == CR) by construction.
    // The pure translation itself (residual plug, banker's rounding) is already
    // covered by `domain::fx::translate::translate_tests`.
}
