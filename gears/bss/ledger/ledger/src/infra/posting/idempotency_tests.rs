//! Tests for [`super::IdempotencyGate::payload_hash`] — the canonical financial-
//! content hash MUST flip on any per-line financial dimension, so a business-key
//! reuse with a differing payload surfaces as a conflict, not a silent replay.

use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use serde_json::json;
use uuid::Uuid;

use super::*;
use time::OffsetDateTime;

fn entry() -> NewEntry {
    NewEntry {
        entry_id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        legal_entity_id: Uuid::from_u128(3),
        period_id: "2026-06".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "biz-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::from_u128(4),
        correlation_id: Uuid::from_u128(5),
        rounding_evidence: json!({}),
        rate_snapshot_ref: None,
    }
}

fn line() -> NewLine {
    NewLine {
        line_id: Uuid::from_u128(10),
        payer_tenant_id: Uuid::from_u128(2),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::from_u128(20),
        account_class: AccountClass::Ar,
        gl_code: None,
        side: Side::Debit,
        amount_minor: 0,
        currency: "USD".to_owned(),
        currency_scale: 2,
        invoice_id: None,
        due_date: None,
        revenue_stream: None,
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: Some(100),
        functional_currency: Some("USD".to_owned()),
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

/// A functional-only line (`amount_minor == 0`) differing only in its
/// `functional_amount_minor` must change the hash — otherwise a corrected
/// re-post is silently swallowed as a replay instead of flagged as an
/// `IDEMPOTENCY_PAYLOAD_CONFLICT`.
#[test]
fn payload_hash_distinguishes_functional_amount() {
    let e = entry();
    let base = line();
    let mut other = base.clone();
    other.functional_amount_minor = Some(200);
    assert_ne!(
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&base)),
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&other)),
        "differing functional_amount_minor must change the payload hash"
    );
}

/// A differing per-line `revenue_stream` must change the hash.
#[test]
fn payload_hash_distinguishes_revenue_stream() {
    let e = entry();
    let mut a = line();
    a.amount_minor = 100;
    let mut b = a.clone();
    a.revenue_stream = Some("stream-a".to_owned());
    b.revenue_stream = Some("stream-b".to_owned());
    assert_ne!(
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&a)),
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&b)),
        "differing revenue_stream must change the payload hash"
    );
}

/// A differing per-line `credit_grant_event_type` (the wallet sub-grain
/// bucket) must change the hash — else a business-key reuse differing only in
/// the bucket is a silent replay instead of an `IDEMPOTENCY_PAYLOAD_CONFLICT`.
#[test]
fn payload_hash_distinguishes_credit_grant_event_type() {
    let e = entry();
    let mut a = line();
    let mut b = a.clone();
    a.credit_grant_event_type = Some("promo".to_owned());
    b.credit_grant_event_type = Some("referral".to_owned());
    assert_ne!(
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&a)),
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&b)),
        "differing credit_grant_event_type must change the payload hash"
    );
}

/// A differing per-line `ar_status` (the dispute reclass sub-class) must
/// change the hash.
#[test]
fn payload_hash_distinguishes_ar_status() {
    let e = entry();
    let mut a = line();
    let mut b = a.clone();
    a.ar_status = Some("ACTIVE".to_owned());
    b.ar_status = Some("DISPUTED".to_owned());
    assert_ne!(
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&a)),
        IdempotencyGate::payload_hash(&e, std::slice::from_ref(&b)),
        "differing ar_status must change the payload hash"
    );
}
