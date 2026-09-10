//! Tests for [`super`]'s pure `derive_grains` — grain fan-out, delta sign,
//! and the missing-`normal_side` error. (DE1101: kept out of the impl file.)

use super::*;
use bss_ledger_sdk::{MappingStatus, SourceDocType};
use time::OffsetDateTime;

fn entry(tenant: Uuid) -> NewEntry {
    NewEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        legal_entity_id: tenant,
        period_id: "202606".to_owned(),
        entry_currency: "USD".to_owned(),
        source_doc_type: SourceDocType::ManualAdjustment,
        source_business_id: "biz-1".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: OffsetDateTime::now_utc(),
        effective_at: chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
        origin: "SYSTEM".to_owned(),
        posted_by_actor_id: tenant,
        correlation_id: tenant,
        rounding_evidence: serde_json::Value::Null,
        rate_snapshot_ref: None,
    }
}

fn line(account: Uuid, class: AccountClass, side: Side, amount: i64, payer: Uuid) -> NewLine {
    NewLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: payer,
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: account,
        account_class: class,
        gl_code: None,
        side,
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

#[test]
fn ar_line_with_invoice_yields_three_grains() {
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let ar = Uuid::now_v7();
    let mut l = line(ar, AccountClass::Ar, Side::Debit, 1000, payer);
    l.invoice_id = Some("INV-1".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);

    let grains = derive_grains(&entry(tenant), &[l], &normal_sides).unwrap();
    assert_eq!(grains.len(), 3);
    // Sorted: account(0), ar_payer(1), ar_invoice(2).
    assert_eq!(grains[0].table_rank, GrainTable::Account);
    assert_eq!(grains[1].table_rank, GrainTable::ArPayer);
    assert_eq!(grains[2].table_rank, GrainTable::ArInvoice);
    // DR on a DR-normal account → positive delta.
    assert_eq!(grains[0].delta, 1000);
}

#[test]
fn credit_against_dr_normal_is_negative_delta() {
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let ar = Uuid::now_v7();
    let l = line(ar, AccountClass::Ar, Side::Credit, 1500, payer);
    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);

    let grains = derive_grains(&entry(tenant), &[l], &normal_sides).unwrap();
    assert_eq!(grains[0].delta, -1500);
}

#[test]
fn two_lines_on_one_account_coalesce_into_a_single_net_grain() {
    // A balanced entry with two legs on the SAME guarded (AR) account: a credit
    // (−100) then a debit (+150). They share a cache-row identity, so they must
    // collapse to ONE grain carrying the net +50 — not two grains whose first
    // (−100) would falsely trip the no-negative guard on an intermediate state.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let ar = Uuid::now_v7();
    let credit = line(ar, AccountClass::Ar, Side::Credit, 100, payer);
    let debit = line(ar, AccountClass::Ar, Side::Debit, 150, payer);
    let mut normal_sides = HashMap::new();
    normal_sides.insert(ar, Side::Debit);

    let grains = derive_grains(&entry(tenant), &[credit, debit], &normal_sides).unwrap();
    // One account grain + one ar_payer grain (both AR), each netted — not four.
    let account_grains: Vec<_> = grains
        .iter()
        .filter(|g| g.table_rank == GrainTable::Account)
        .collect();
    assert_eq!(account_grains.len(), 1, "same-account legs must coalesce");
    assert_eq!(account_grains[0].delta, 50, "net of −100 then +150");
    let payer_grains: Vec<_> = grains
        .iter()
        .filter(|g| g.table_rank == GrainTable::ArPayer)
        .collect();
    assert_eq!(payer_grains.len(), 1);
    assert_eq!(payer_grains[0].delta, 50);
}

#[test]
fn cr_unallocated_line_yields_an_unallocated_grain() {
    // A CR UNALLOCATED line (CR on a CR-normal unapplied-cash account) fans out
    // to its account grain (0) + the UNALLOCATED grain (3), keyed by payer +
    // currency + account, carrying the normal-side-positive delta.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let unalloc = Uuid::now_v7();
    let l = line(
        unalloc,
        AccountClass::Unallocated,
        Side::Credit,
        1000,
        payer,
    );
    let mut normal_sides = HashMap::new();
    normal_sides.insert(unalloc, Side::Credit);

    let grains = derive_grains(&entry(tenant), &[l], &normal_sides).unwrap();
    // account(0) + unallocated(3).
    assert_eq!(grains.len(), 2);
    let unalloc_grain = grains
        .iter()
        .find(|g| g.table_rank == GrainTable::Unallocated)
        .expect("an UNALLOCATED-rank grain must be emitted");
    assert_eq!(unalloc_grain.account_id, unalloc);
    assert_eq!(unalloc_grain.payer_tenant_id, payer);
    assert_eq!(unalloc_grain.currency, "USD");
    assert_eq!(unalloc_grain.account_class, AccountClass::Unallocated);
    // CR on a CR-normal account → positive delta.
    assert_eq!(unalloc_grain.delta, 1000);
}

#[test]
fn missing_normal_side_is_an_error() {
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let ar = Uuid::now_v7();
    let l = line(ar, AccountClass::Ar, Side::Debit, 1000, payer);
    let err = derive_grains(&entry(tenant), &[l], &HashMap::new()).unwrap_err();
    assert_eq!(err, ProjectError::MissingNormalSide(ar));
}

#[test]
fn cr_reusable_credit_line_yields_a_keyed_credit_grain() {
    // A CR REUSABLE_CREDIT line (CR on a CR-normal wallet account) fans out to
    // its account grain (0) + the REUSABLE_CREDIT grain (4), keyed by payer +
    // currency + account + the credit-grant event type, carrying the
    // normal-side-positive delta and the entry's posted-at as the first-granted
    // recency stamp.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let e = entry(tenant);
    let posted_at = e.posted_at_utc;
    let mut l = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        1000,
        payer,
    );
    l.credit_grant_event_type = Some("promo".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(wallet, Side::Credit);

    let grains = derive_grains(&e, &[l], &normal_sides).unwrap();
    // account(0) + reusable_credit(4).
    assert_eq!(grains.len(), 2);
    let credit_grain = grains
        .iter()
        .find(|g| g.table_rank == GrainTable::ReusableCredit)
        .expect("a REUSABLE_CREDIT-rank grain must be emitted");
    assert_eq!(credit_grain.account_id, wallet);
    assert_eq!(credit_grain.payer_tenant_id, payer);
    assert_eq!(credit_grain.currency, "USD");
    assert_eq!(credit_grain.account_class, AccountClass::ReusableCredit);
    assert_eq!(credit_grain.credit_grant_event_type, "promo");
    // CR on a CR-normal account → positive delta.
    assert_eq!(credit_grain.delta, 1000);
    // First-write-wins recency stamp = the entry's posted-at.
    assert_eq!(credit_grain.first_granted_at, Some(posted_at));
}

#[test]
fn dr_reusable_credit_line_is_negative_delta() {
    // A DR REUSABLE_CREDIT line (a wallet spend) nets a negative delta against
    // the CR-normal wallet — the app-level overdraw guard in the upsert is what
    // rejects it below zero.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let mut l = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Debit,
        400,
        payer,
    );
    l.credit_grant_event_type = Some("promo".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(wallet, Side::Credit);

    let grains = derive_grains(&entry(tenant), &[l], &normal_sides).unwrap();
    let credit_grain = grains
        .iter()
        .find(|g| g.table_rank == GrainTable::ReusableCredit)
        .expect("a REUSABLE_CREDIT-rank grain must be emitted");
    // DR on a CR-normal account → negative delta.
    assert_eq!(credit_grain.delta, -400);
}

#[test]
fn reusable_credit_without_event_type_is_rejected() {
    // A REUSABLE_CREDIT line that reaches projection without its sub-grain bucket
    // would key a phantom "" sub-balance — reject rather than silently default to
    // "" (the DB NOT-NULL CHECK tests NULL, not "", so it would not catch it).
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let mut l = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        1000,
        payer,
    );
    l.credit_grant_event_type = None;
    let line_id = l.line_id;
    let mut normal_sides = HashMap::new();
    normal_sides.insert(wallet, Side::Credit);

    let err = derive_grains(&entry(tenant), &[l], &normal_sides).unwrap_err();
    assert_eq!(err, ProjectError::MissingCreditEventType(line_id));
}

#[test]
fn grain_lock_order_ranks_are_pinned() {
    // Pins the ACTUAL deadlock-free balance-cache lock order. The code is the
    // source of truth (the design doc was reconciled to it; payer is ranked
    // before invoice). A reorder is a cross-slice
    // deadlock risk (Slice 3 acquires these same grains) and must be deliberate —
    // a reorder fails the BUILD (const assertions), not just the test. Slice 4
    // extends the chain with the recognition ranks (6/7) it added below tax.
    // `GrainTable` is `derive(Ord)`, so the lock order IS the declaration order;
    // these `as u8` discriminant checks still fail the BUILD on an accidental
    // reorder. The recognition tables (schedule/segment) extend the chain below
    // `Tax` when their variants are added.
    const {
        assert!(
            (GrainTable::Account as u8) < (GrainTable::ArPayer as u8),
            "account must lock before ar_payer"
        );
        assert!(
            (GrainTable::ArPayer as u8) < (GrainTable::ArInvoice as u8),
            "ar_payer must lock before ar_invoice"
        );
        assert!(
            (GrainTable::ArInvoice as u8) < (GrainTable::Unallocated as u8),
            "ar_invoice before unallocated"
        );
        assert!(
            (GrainTable::Unallocated as u8) < (GrainTable::ReusableCredit as u8),
            "unallocated before reusable_credit"
        );
        assert!(
            (GrainTable::ReusableCredit as u8) < (GrainTable::Tax as u8),
            "reusable_credit before tax"
        );
    }
}

#[test]
fn different_credit_grant_event_types_do_not_coalesce() {
    // The credit-grant event type is a PK dim of the wallet sub-grain, so two
    // lines on the SAME account/currency but DIFFERENT event types map to two
    // distinct cache rows — they must stay two grains, not collapse.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let mut promo = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        100,
        payer,
    );
    promo.credit_grant_event_type = Some("promo".to_owned());
    let mut referral = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        250,
        payer,
    );
    referral.credit_grant_event_type = Some("referral".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(wallet, Side::Credit);

    let grains = derive_grains(&entry(tenant), &[promo, referral], &normal_sides).unwrap();
    let credit_grain_count = grains
        .iter()
        .filter(|g| g.table_rank == GrainTable::ReusableCredit)
        .count();
    assert_eq!(
        credit_grain_count, 2,
        "distinct event types must not coalesce"
    );
}

#[test]
fn same_credit_grant_event_type_coalesces_into_one_net_grain() {
    // Two lines on the SAME account/currency/event-type share a cache-row
    // identity, so they collapse to ONE grain carrying the net delta — exactly
    // like the same-account AR coalescing.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let mut grant = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        500,
        payer,
    );
    grant.credit_grant_event_type = Some("promo".to_owned());
    let mut spend = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Debit,
        200,
        payer,
    );
    spend.credit_grant_event_type = Some("promo".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(wallet, Side::Credit);

    let grains = derive_grains(&entry(tenant), &[grant, spend], &normal_sides).unwrap();
    let credit_grains: Vec<_> = grains
        .iter()
        .filter(|g| g.table_rank == GrainTable::ReusableCredit)
        .collect();
    assert_eq!(credit_grains.len(), 1, "same event type must coalesce");
    // CR +500 then DR −200 on a CR-normal account → net +300.
    assert_eq!(credit_grains[0].delta, 300);
}

#[test]
fn reusable_credit_grain_sorts_between_unallocated_and_tax() {
    // The canonical lock order places the wallet sub-grain (rank 4) strictly
    // after unallocated (3) and before tax (5). A single entry exercising an
    // UNALLOCATED line, a REUSABLE_CREDIT line, and a TAX_PAYABLE line must emit
    // those three grains in that relative order after the sort.
    let tenant = Uuid::now_v7();
    let payer = Uuid::now_v7();
    let unalloc = Uuid::now_v7();
    let wallet = Uuid::now_v7();
    let tax = Uuid::now_v7();
    let unalloc_line = line(unalloc, AccountClass::Unallocated, Side::Credit, 100, payer);
    let mut credit_line = line(
        wallet,
        AccountClass::ReusableCredit,
        Side::Credit,
        100,
        payer,
    );
    credit_line.credit_grant_event_type = Some("promo".to_owned());
    let mut tax_line = line(tax, AccountClass::TaxPayable, Side::Credit, 100, payer);
    tax_line.tax_jurisdiction = Some("US-CA".to_owned());
    tax_line.tax_filing_period = Some("2026Q2".to_owned());
    let mut normal_sides = HashMap::new();
    normal_sides.insert(unalloc, Side::Credit);
    normal_sides.insert(wallet, Side::Credit);
    normal_sides.insert(tax, Side::Credit);

    let grains = derive_grains(
        &entry(tenant),
        &[unalloc_line, credit_line, tax_line],
        &normal_sides,
    )
    .unwrap();
    let ranks: Vec<GrainTable> = grains
        .iter()
        .map(|g| g.table_rank)
        .filter(|r| {
            *r == GrainTable::Unallocated
                || *r == GrainTable::ReusableCredit
                || *r == GrainTable::Tax
        })
        .collect();
    assert_eq!(
        ranks,
        vec![
            GrainTable::Unallocated,
            GrainTable::ReusableCredit,
            GrainTable::Tax
        ],
        "lock order: unallocated → reusable_credit → tax"
    );
}
