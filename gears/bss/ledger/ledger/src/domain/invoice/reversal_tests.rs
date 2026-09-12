//! Tests for the reversal + `MAPPING_CORRECTION` flow.

use bss_ledger_sdk::{AccountClass, LineView, MappingStatus};
use chrono::NaiveDate;

use super::*;
use time::OffsetDateTime;

fn naive(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn line(account: Uuid, class: AccountClass, side: Side, amount: i64) -> LineView {
    LineView {
        line_id: Uuid::now_v7(),
        entry_id: Uuid::now_v7(),
        payer_tenant_id: Uuid::now_v7(),
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
        amount_minor: amount,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: Some("INV-1".to_owned()),
        due_date: Some(naive(2026, 7, 1)),
        revenue_stream: if class == AccountClass::Revenue {
            Some("subscription".to_owned())
        } else {
            None
        },
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        ar_status: None,
    }
}

/// Like [`line`] but cross-currency: carries a functional (EUR) translation, so a
/// reversal must copy it onto the flipped leg (the carry-forward fix).
fn fx_line(
    account: Uuid,
    class: AccountClass,
    side: Side,
    amount: i64,
    functional: i64,
) -> LineView {
    LineView {
        functional_amount_minor: Some(functional),
        functional_currency: Some("EUR".to_owned()),
        ..line(account, class, side, amount)
    }
}

/// An original `INVOICE_POST` entry: DR AR 1200 / CR Revenue 1000 / CR Tax 200.
fn original_invoice() -> EntryView {
    let ar = Uuid::now_v7();
    let rev = Uuid::now_v7();
    let tax = Uuid::now_v7();
    EntryView {
        entry_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "INV-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: now(),
        effective_at: naive(2026, 6, 1),
        posted_by_actor_id: Uuid::now_v7(),
        origin: "SYSTEM".to_owned(),
        correlation_id: Uuid::now_v7(),
        created_seq: 1,
        lines: vec![
            line(ar, AccountClass::Ar, Side::Debit, 1200),
            line(rev, AccountClass::Revenue, Side::Credit, 1000),
            line(tax, AccountClass::TaxPayable, Side::Credit, 200),
        ],
    }
}

#[test]
fn reversal_flips_sides_keeps_amounts_positive_and_sets_reverses() {
    let original = original_invoice();
    let actor = Uuid::now_v7();
    let corr = Uuid::now_v7();
    let reversal = build_reversal(
        &original,
        "202607".to_owned(),
        naive(2026, 7, 2),
        actor,
        corr,
    )
    .expect("reversal of an invoice-post must build");

    assert_eq!(reversal.source_doc_type, SourceDocType::Reversal);
    assert_eq!(
        reversal.reverses_entry_id,
        Some(original.entry_id),
        "reverses_entry_id must point at the original"
    );
    assert_eq!(
        reversal.reverses_period_id.as_deref(),
        Some("202606"),
        "reverses_period_id must carry the original's period"
    );
    assert_eq!(
        reversal.period_id, "202607",
        "the reversal posts into the supplied period"
    );
    assert_eq!(
        reversal.source_business_id,
        format!("reverses={}", original.entry_id)
    );

    // Same accounts, flipped sides, positive amounts.
    assert_eq!(reversal.lines.len(), original.lines.len());
    for (orig, rev) in original.lines.iter().zip(reversal.lines.iter()) {
        assert_eq!(rev.account_id, orig.account_id, "same account");
        assert_eq!(rev.amount_minor, orig.amount_minor, "amount unchanged");
        assert!(rev.amount_minor > 0, "reversal amount stays positive");
        let flipped = match orig.side {
            Side::Debit => Side::Credit,
            Side::Credit => Side::Debit,
        };
        assert_eq!(rev.side, flipped, "side flipped");
    }

    // The reversal nets to zero on its own (DR 1000 + DR 200 / CR 1200).
    let net: i128 = reversal
        .lines
        .iter()
        .map(|l| match l.side {
            Side::Debit => i128::from(l.amount_minor),
            Side::Credit => -i128::from(l.amount_minor),
        })
        .sum();
    assert_eq!(net, 0, "the reversal is itself balanced");
}

#[test]
fn reversal_carries_functional_forward_and_nets_to_zero() {
    // A cross-currency original (USD transaction, EUR functional at 0.9): DR AR
    // 1200/1080 / CR Revenue 1000/900 / CR Tax 200/180. The reversal must copy each
    // leg's functional (positive) onto the flipped side so the functional column
    // nets to zero — the fix for the silent transaction-vs-functional drift on a
    // cross-currency reversal (it must NOT post functional-NULL).
    let ar = Uuid::now_v7();
    let rev_acct = Uuid::now_v7();
    let tax = Uuid::now_v7();
    let mut original = original_invoice();
    original.lines = vec![
        fx_line(ar, AccountClass::Ar, Side::Debit, 1200, 1080),
        fx_line(rev_acct, AccountClass::Revenue, Side::Credit, 1000, 900),
        fx_line(tax, AccountClass::TaxPayable, Side::Credit, 200, 180),
    ];

    let reversal = build_reversal(
        &original,
        "202607".to_owned(),
        naive(2026, 7, 2),
        Uuid::now_v7(),
        Uuid::now_v7(),
    )
    .expect("cross-currency reversal must build");

    // Every leg carries the ORIGINAL functional (positive) + currency, side flipped.
    for (orig, rev) in original.lines.iter().zip(reversal.lines.iter()) {
        assert_eq!(
            rev.functional_amount_minor, orig.functional_amount_minor,
            "functional carried at the original rate (positive, unchanged)"
        );
        assert_eq!(
            rev.functional_currency.as_deref(),
            Some("EUR"),
            "functional currency carried"
        );
    }

    // Functional column nets to zero (DR 900 + DR 180 / CR 1080) — no drift, no
    // synthesized FX gain/loss.
    let func_net: i128 = reversal
        .lines
        .iter()
        .map(|l| {
            let f = i128::from(
                l.functional_amount_minor
                    .expect("cross-ccy leg carries functional"),
            );
            match l.side {
                Side::Debit => f,
                Side::Credit => -f,
            }
        })
        .sum();
    assert_eq!(func_net, 0, "the reversal's functional column is balanced");
}

#[test]
fn reverse_of_a_reversal_is_rejected() {
    let mut already_a_reversal = original_invoice();
    already_a_reversal.source_doc_type = SourceDocType::Reversal;
    let err = build_reversal(
        &already_a_reversal,
        "202607".to_owned(),
        naive(2026, 7, 2),
        Uuid::now_v7(),
        Uuid::now_v7(),
    )
    .expect_err("reversing a reversal must be rejected");
    assert_eq!(err, ReversalError::CannotReverseReversal);
}

#[test]
fn reverse_of_an_entry_with_a_reusable_credit_line_is_rejected() {
    // The read-back `LineView` does not carry `credit_grant_event_type`, so a
    // faithful reversal of a REUSABLE_CREDIT line cannot be reconstructed — the
    // guard must fail fast rather than abort at the DB CHECK.
    let mut with_credit = original_invoice();
    with_credit.lines.push(line(
        Uuid::now_v7(),
        AccountClass::ReusableCredit,
        Side::Credit,
        500,
    ));
    let err = build_reversal(
        &with_credit,
        "202607".to_owned(),
        naive(2026, 7, 2),
        Uuid::now_v7(),
        Uuid::now_v7(),
    )
    .expect_err("reversing an entry with a REUSABLE_CREDIT line must be rejected");
    assert_eq!(err, ReversalError::CreditGrantNotReconstructible);
}

#[test]
fn correction_id_is_deterministic_for_the_same_pair() {
    let original = Uuid::now_v7();
    let reversal = Uuid::now_v7();
    assert_eq!(
        correction_id(original, reversal),
        correction_id(original, reversal),
        "the same (original, reversal) pair must hash identically"
    );
    // 64 hex chars (SHA-256).
    assert_eq!(correction_id(original, reversal).len(), 64);
}

#[test]
fn correction_id_differs_for_different_inputs() {
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let c = Uuid::now_v7();
    assert_ne!(
        correction_id(a, b),
        correction_id(a, c),
        "a different reversal id must yield a different correction id"
    );
    // Order-sensitive: swapping the pair changes the id.
    assert_ne!(
        correction_id(a, b),
        correction_id(b, a),
        "correction_id must be order-sensitive"
    );
}

#[test]
fn mapping_correction_keys_on_invoice_and_correction_id() {
    let original = original_invoice();
    let reversal_entry_id = Uuid::now_v7();
    let correction = correction_id(original.entry_id, reversal_entry_id);
    let corrected = build_mapping_correction(
        &original,
        reversal_entry_id,
        "INV-1",
        "202607".to_owned(),
        naive(2026, 7, 2),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Vec::new(),
    );
    assert_eq!(corrected.source_doc_type, SourceDocType::MappingCorrection);
    assert_eq!(
        corrected.source_business_id,
        format!("INV-1:{correction}"),
        "MAPPING_CORRECTION keys on invoice_id:correction_id"
    );
    assert_eq!(
        corrected.reverses_entry_id,
        Some(reversal_entry_id),
        "the correction points back at the reversal it follows"
    );
}
