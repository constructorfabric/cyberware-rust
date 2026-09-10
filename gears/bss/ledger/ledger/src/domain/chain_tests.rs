//! Unit tests for the canonical tamper-evidence chain encoder ([`super`]):
//! determinism, line-order independence, NULL-safety, field sensitivity, the
//! PII/correlation/rounding-evidence exclusions, and the §11 byte-repro vector.

use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;

use crate::domain::instant::from_unix;
use serde_json::json;
use uuid::Uuid;

use super::{chain_row_hash, genesis_prev_hash};
use crate::domain::model::{NewEntry, NewLine};

fn entry() -> NewEntry {
    NewEntry {
        entry_id: Uuid::from_u128(1),
        tenant_id: Uuid::from_u128(2),
        legal_entity_id: Uuid::from_u128(3),
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::InvoicePost,
        source_business_id: "biz-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: from_unix(1_750_000_000, 0).unwrap(),
        effective_at: NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: Uuid::from_u128(4),
        correlation_id: Uuid::from_u128(5),
        rounding_evidence: json!({}),
        rate_snapshot_ref: None,
    }
}

fn line(id: u128, amount: i64) -> NewLine {
    NewLine {
        line_id: Uuid::from_u128(id),
        payer_tenant_id: Uuid::from_u128(2),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::from_u128(20),
        account_class: AccountClass::Ar,
        gl_code: None,
        side: Side::Debit,
        amount_minor: amount,
        currency: "USD".to_owned(),
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

const PREV: [u8; 32] = [7u8; 32];

#[test]
fn deterministic() {
    let (e, ls) = (entry(), vec![line(10, 100), line(11, -100)]);
    assert_eq!(
        chain_row_hash(&e, &ls, &PREV),
        chain_row_hash(&e, &ls, &PREV)
    );
}

#[test]
fn line_input_order_independent() {
    let e = entry();
    let a = vec![line(10, 100), line(11, -100)];
    let b = vec![line(11, -100), line(10, 100)];
    assert_eq!(chain_row_hash(&e, &a, &PREV), chain_row_hash(&e, &b, &PREV));
}

#[test]
fn prev_hash_sensitive() {
    let (e, ls) = (entry(), vec![line(10, 100)]);
    assert_ne!(
        chain_row_hash(&e, &ls, &[1u8; 32]),
        chain_row_hash(&e, &ls, &[2u8; 32])
    );
}

#[test]
fn amount_sensitive() {
    let e = entry();
    assert_ne!(
        chain_row_hash(&e, &[line(10, 100)], &PREV),
        chain_row_hash(&e, &[line(10, 101)], &PREV)
    );
}

#[test]
fn ar_status_covered() {
    // `ar_status` is financially binding (Slice 2 chargeback reclass routes a
    // disputed sub-balance on `DISPUTED`, projector.rs); a tamper flipping it
    // MUST change the row hash so the Verifier catches it (design §4.2, Rev2 B-8).
    let e = entry();
    let mut active = line(10, 100);
    active.ar_status = Some("ACTIVE".to_owned());
    let mut disputed = line(10, 100);
    disputed.ar_status = Some("DISPUTED".to_owned());
    let none = line(10, 100); // ar_status: None
    let h_active = chain_row_hash(&e, &[active], &PREV);
    let h_disputed = chain_row_hash(&e, &[disputed], &PREV);
    let h_none = chain_row_hash(&e, &[none], &PREV);
    assert_ne!(
        h_active, h_disputed,
        "ACTIVE vs DISPUTED must change the hash"
    );
    assert_ne!(h_none, h_disputed, "None vs DISPUTED must change the hash");
    assert_ne!(h_none, h_active, "None vs ACTIVE must change the hash");
}

#[test]
fn excludes_correlation_and_rounding_evidence() {
    let ls = vec![line(10, 100)];
    let mut e2 = entry();
    e2.correlation_id = Uuid::from_u128(999);
    e2.rounding_evidence = json!({"x": 1});
    assert_eq!(
        chain_row_hash(&entry(), &ls, &PREV),
        chain_row_hash(&e2, &ls, &PREV)
    );
}

#[test]
fn null_safe_none_vs_empty_string() {
    let e = entry();
    let mut none_gl = line(10, 100);
    none_gl.gl_code = None;
    let mut empty_gl = line(10, 100);
    empty_gl.gl_code = Some(String::new());
    assert_ne!(
        chain_row_hash(&e, &[none_gl], &PREV),
        chain_row_hash(&e, &[empty_gl], &PREV)
    );
}

#[test]
fn genesis_is_tenant_bound() {
    assert_ne!(
        genesis_prev_hash(Uuid::from_u128(1)),
        genesis_prev_hash(Uuid::from_u128(2))
    );
}

/// The §11 byte-reproducibility vector. REGENERATE only on an intentional
/// encoding change: run once, paste the printed hex into `EXPECTED`. A silent
/// change means the chain encoding drifted — exactly what this guards.
#[test]
fn byte_reproducibility_vector() {
    use std::fmt::Write as _;
    const EXPECTED: &str = "b943d061aa92913945784ef003bc6b43cc8c9a800fdd758bbe1a53ee39d994ec";
    let e = entry();
    let ls = vec![line(10, 100), line(11, -100)];
    let digest = chain_row_hash(&e, &ls, &[0u8; 32]);
    let mut hex = String::with_capacity(64);
    for b in digest {
        let _ = write!(hex, "{b:02x}");
    }
    assert_eq!(hex, EXPECTED, "tamper-chain encoding changed — got {hex}");
}
