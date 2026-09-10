//! Tests for the broker-absent (`noop`) publisher path.
//!
//! The noop publisher must accept every event method and return `Ok(())` (or
//! return without panicking for fire-and-forget calls) so a post never fails
//! just because events are disabled.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::LedgerEventPublisher;
use crate::domain::instant::from_unix;
use crate::infra::events::payloads::{
    AffectedItem, AlarmCategory, AlarmSeverity, LedgerEntryPosted, LedgerEntryReversed,
    LedgerInvariantAlarm, LedgerLineSummary,
};

fn sample_posted() -> LedgerEntryPosted {
    LedgerEntryPosted {
        entry_id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
        tenant_id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
        legal_entity_id: Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333),
        period_id: "2026-06".to_owned(),
        source_doc_type: "INVOICE".to_owned(),
        source_business_id: "inv-42".to_owned(),
        posted_at_utc: from_unix(1_700_000_000, 0).expect("ts"),
        created_seq: 7,
        lines: vec![
            LedgerLineSummary {
                account_class: "AR".to_owned(),
                side: "DR".to_owned(),
                amount_minor: 1_000,
                currency: "USD".to_owned(),
                currency_scale: 2,
            },
            LedgerLineSummary {
                account_class: "REVENUE".to_owned(),
                side: "CR".to_owned(),
                amount_minor: 1_000,
                currency: "USD".to_owned(),
                currency_scale: 2,
            },
        ],
    }
}

fn sample_alarm() -> LedgerInvariantAlarm {
    LedgerInvariantAlarm {
        category: AlarmCategory::NegativeBalanceViolation,
        severity: AlarmSeverity::Critical,
        tenant_id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
        scope: "tenant:t/flow:f/business:b".to_owned(),
        code: "NEGATIVE_BALANCE_VIOLATION".to_owned(),
        detail: "balance would go negative".to_owned(),
        affected: vec![AffectedItem {
            id: Uuid::from_u128(0x4444_4444_4444_4444_4444_4444_4444_4444).to_string(),
            currency: "USD".to_owned(),
            expected_minor: 0,
            actual_minor: -100,
        }],
    }
}

/// Open an in-memory SQLite `DBProvider`. The noop publisher never writes to
/// the DB, so no migrations are needed.
#[test]
fn ledger_entry_posted_serializes_posted_at_as_rfc3339() {
    let value = serde_json::to_value(sample_posted()).expect("serialize");
    let posted = value["posted_at_utc"]
        .as_str()
        .expect("posted_at_utc must be an RFC3339 string, not a time component tuple");
    // **The bytes, not the shape.** This field is read by other gears off the
    // outbox, so the contract is the exact rendering and not "some RFC 3339".
    // Asserting `contains('T')` and a `Z`-or-`+` tail passed every renderer the
    // gear has ever had — `+00:00`, nine digits, `time`'s minimal `.1` — so it
    // could not have caught the one change that moves the event's bytes.
    assert_eq!(
        posted, "2023-11-14T22:13:20.000000Z",
        "posted_at_utc is `format_rfc3339`'s rendering, byte for byte"
    );
}

async fn sqlite_provider() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("open in-memory sqlite");
    DBProvider::<DbError>::new(db)
}

#[tokio::test]
async fn noop_publish_entry_posted_returns_ok() {
    // The noop publisher returns `Ok(())` before touching the transaction, so
    // an un-migrated in-memory DB is sufficient to obtain a `DbTx`.
    let publisher = LedgerEventPublisher::noop();
    let ctx = SecurityContext::anonymous();
    let provider = sqlite_provider().await;

    let event = sample_posted();
    // The transaction closure must be `FnOnce` — move owned values in.
    let result = provider
        .transaction(|txn| {
            Box::pin(async move {
                publisher
                    .publish_entry_posted(&ctx, txn, event)
                    .await
                    .map_err(|e| toolkit_db::DbError::Sea(sea_orm::DbErr::Custom(e.to_string())))
            })
        })
        .await;

    assert!(
        result.is_ok(),
        "noop publisher must return Ok for publish_entry_posted: {result:?}"
    );
}

#[tokio::test]
async fn noop_publish_entry_reversed_returns_ok() {
    // The noop publisher returns `Ok(())` before touching the transaction, so an
    // un-migrated in-memory DB is sufficient to obtain a `DbTx`.
    let publisher = LedgerEventPublisher::noop();
    let ctx = SecurityContext::anonymous();
    let provider = sqlite_provider().await;

    let event = LedgerEntryReversed {
        entry_id: Uuid::from_u128(0x8888_8888_8888_8888_8888_8888_8888_8888),
        reverses_entry_id: Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111),
        tenant_id: Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222),
        reason: "duplicate charge backed out".to_owned(),
    };
    let result = provider
        .transaction(|txn| {
            Box::pin(async move {
                publisher
                    .publish_entry_reversed(&ctx, txn, event)
                    .await
                    .map_err(|e| toolkit_db::DbError::Sea(sea_orm::DbErr::Custom(e.to_string())))
            })
        })
        .await;

    assert!(
        result.is_ok(),
        "noop publisher must return Ok for publish_entry_reversed: {result:?}"
    );
}

#[tokio::test]
async fn noop_emit_invariant_alarm_does_not_panic() {
    // Fire-and-forget: noop path logs a warning and returns. No Err, no panic.
    let publisher = LedgerEventPublisher::noop();
    let ctx = SecurityContext::anonymous();
    publisher.emit_invariant_alarm(&ctx, sample_alarm()).await;
    // Reaching here without a panic means the noop path is correct.
}

#[tokio::test]
async fn noop_emit_invariant_alarm_all_categories() {
    // Ensure every AlarmCategory can be passed to the noop publisher without panic.
    let publisher = LedgerEventPublisher::noop();
    let ctx = SecurityContext::anonymous();
    let tenant_id = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);

    // Drive EVERY category through the noop path (the canonical `ALL` slice, so a
    // new variant is exercised automatically) — none may panic.
    for category in AlarmCategory::ALL.iter().copied() {
        let code = category.as_str().to_owned();
        let alarm = LedgerInvariantAlarm {
            category,
            severity: AlarmSeverity::Warn,
            tenant_id,
            scope: "tenant:t".to_owned(),
            code,
            detail: "test".to_owned(),
            affected: vec![],
        };
        publisher.emit_invariant_alarm(&ctx, alarm).await;
    }
}
