//! Unit tests for the OData field→column mappers in `odata_mapping.rs`.
//!
//! `Column` enums emitted by `DeriveEntityModel` implement `IdenStatic` but
//! not `PartialEq`, so we compare via `IdenStatic::as_str()` (the DB column
//! name). This is a stronger check: it validates the actual SQL identifier, not
//! just the enum discriminant.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::similar_names,
    clippy::inconsistent_struct_constructor
)]

use chrono::NaiveDate;
use sea_orm::IdenStatic;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::infra::storage::entity::{
    account_balance, credit_note, debit_note, dispute, exception_queue, journal_entry,
    journal_line, recognition_run, refund, tenant_account,
};
use crate::odata::{
    AccountInfoFilterField, BalanceFilterField, CreditNoteFilterField, DebitNoteFilterField,
    DisputeFilterField, ExceptionFilterField, JournalEntryFilterField, JournalLineFilterField,
    RecognitionRunFilterField, RefundFilterField,
};

use super::{
    AccountInfoODataMapper, BalanceColumn, BalanceODataMapper, CreditNoteColumn,
    CreditNoteODataMapper, DebitNoteColumn, DebitNoteODataMapper, DisputeColumn,
    DisputeODataMapper, ExceptionColumn, ExceptionODataMapper, JournalEntryColumn,
    JournalEntryODataMapper, JournalLineColumn, JournalLineODataMapper, RecognitionRunColumn,
    RecognitionRunODataMapper, RefundColumn, RefundODataMapper, TenantAccountColumn,
};
use crate::domain::instant::from_unix;
use toolkit_db::odata::sea_orm_filter::{FieldToColumn, ODataFieldMapping};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sample_tenant_account() -> tenant_account::Model {
    tenant_account::Model {
        account_id: Uuid::from_u128(0x0A),
        tenant_id: Uuid::from_u128(0x1A),
        legal_entity_id: Uuid::from_u128(0x2A),
        account_class: "AR".to_owned(),
        currency: "USD".to_owned(),
        revenue_stream: Some("SAAS".to_owned()),
        normal_side: "DR".to_owned(),
        may_go_negative: false,
        lifecycle_state: "active".to_owned(),
    }
}

fn sample_journal_line() -> journal_line::Model {
    journal_line::Model {
        line_id: Uuid::from_u128(0x0B),
        entry_id: Uuid::from_u128(0x1B),
        tenant_id: Uuid::from_u128(0x2B),
        period_id: "2025-01".to_owned(),
        payer_tenant_id: Uuid::from_u128(0x3B),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::from_u128(0x4B),
        account_class: "AR".to_owned(),
        gl_code: None,
        side: "DR".to_owned(),
        amount_minor: 5000,
        currency: "EUR".to_owned(),
        currency_scale: 2,
        invoice_id: Some("INV-001".to_owned()),
        due_date: None,
        revenue_stream: None,
        mapping_status: "RESOLVED".to_owned(),
        functional_amount_minor: None,
        functional_currency: None,
        rate_snapshot_ref: None,
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

fn sample_account_balance() -> account_balance::Model {
    account_balance::Model {
        tenant_id: Uuid::from_u128(0xA1),
        account_id: Uuid::from_u128(0xB1),
        currency: "GBP".to_owned(),
        account_class: "REVENUE".to_owned(),
        normal_side: "CR".to_owned(),
        balance_minor: 9000,
        functional_balance_minor: None,
        functional_currency: None,
        last_entry_seq: Some(42),
        version: 3,
    }
}

// ---------------------------------------------------------------------------
// AccountInfoODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn account_info_map_account_id() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::AccountId).as_str(),
        TenantAccountColumn::AccountId.as_str(),
    );
}

#[test]
fn account_info_map_account_class() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::AccountClass).as_str(),
        TenantAccountColumn::AccountClass.as_str(),
    );
}

#[test]
fn account_info_map_currency() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::Currency).as_str(),
        TenantAccountColumn::Currency.as_str(),
    );
}

#[test]
fn account_info_map_revenue_stream() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::RevenueStream).as_str(),
        TenantAccountColumn::RevenueStream.as_str(),
    );
}

#[test]
fn account_info_map_lifecycle_state() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::LifecycleState).as_str(),
        TenantAccountColumn::LifecycleState.as_str(),
    );
}

// ---------------------------------------------------------------------------
// AccountInfoODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn account_info_cursor_account_id_is_uuid() {
    let model = sample_tenant_account();
    let val =
        AccountInfoODataMapper::extract_cursor_value(&model, AccountInfoFilterField::AccountId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(model.account_id)));
}

#[test]
fn account_info_cursor_account_class_is_string() {
    let model = sample_tenant_account();
    let val =
        AccountInfoODataMapper::extract_cursor_value(&model, AccountInfoFilterField::AccountClass);
    assert_eq!(val, sea_orm::Value::String(Some("AR".to_owned())));
}

#[test]
fn account_info_cursor_currency_is_string() {
    let model = sample_tenant_account();
    let val =
        AccountInfoODataMapper::extract_cursor_value(&model, AccountInfoFilterField::Currency);
    assert_eq!(val, sea_orm::Value::String(Some("USD".to_owned())),);
}

#[test]
fn account_info_cursor_revenue_stream_some() {
    let model = sample_tenant_account();
    let val =
        AccountInfoODataMapper::extract_cursor_value(&model, AccountInfoFilterField::RevenueStream);
    assert_eq!(val, sea_orm::Value::String(Some("SAAS".to_owned())),);
}

#[test]
fn account_info_cursor_revenue_stream_none_is_null_string() {
    let mut model = sample_tenant_account();
    model.revenue_stream = None;
    let val =
        AccountInfoODataMapper::extract_cursor_value(&model, AccountInfoFilterField::RevenueStream);
    assert_eq!(val, sea_orm::Value::String(None));
}

#[test]
fn account_info_cursor_lifecycle_state_is_string() {
    let model = sample_tenant_account();
    let val = AccountInfoODataMapper::extract_cursor_value(
        &model,
        AccountInfoFilterField::LifecycleState,
    );
    assert_eq!(val, sea_orm::Value::String(Some("active".to_owned())),);
}

// ---------------------------------------------------------------------------
// JournalLineODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn journal_line_map_line_id() {
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::LineId).as_str(),
        JournalLineColumn::LineId.as_str(),
    );
}

#[test]
fn journal_line_map_payer_tenant_id() {
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::PayerTenantId).as_str(),
        JournalLineColumn::PayerTenantId.as_str(),
    );
}

#[test]
fn journal_line_map_account_class() {
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::AccountClass).as_str(),
        JournalLineColumn::AccountClass.as_str(),
    );
}

#[test]
fn journal_line_map_period_id() {
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::PeriodId).as_str(),
        JournalLineColumn::PeriodId.as_str(),
    );
}

#[test]
fn journal_line_map_invoice_id() {
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::InvoiceId).as_str(),
        JournalLineColumn::InvoiceId.as_str(),
    );
}

// ---------------------------------------------------------------------------
// JournalLineODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn journal_line_cursor_line_id_is_uuid() {
    let model = sample_journal_line();
    let val = JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::LineId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(model.line_id)));
}

#[test]
fn journal_line_cursor_payer_tenant_id_is_uuid() {
    let model = sample_journal_line();
    let val =
        JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::PayerTenantId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(Uuid::from_u128(0x3B))),);
}

#[test]
fn journal_line_cursor_account_class_is_string() {
    let model = sample_journal_line();
    let val =
        JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::AccountClass);
    assert_eq!(val, sea_orm::Value::String(Some("AR".to_owned())));
}

#[test]
fn journal_line_cursor_period_id_is_string() {
    let model = sample_journal_line();
    let val =
        JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::PeriodId);
    assert_eq!(val, sea_orm::Value::String(Some("2025-01".to_owned())),);
}

#[test]
fn journal_line_cursor_invoice_id_some() {
    let model = sample_journal_line();
    let val =
        JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::InvoiceId);
    assert_eq!(val, sea_orm::Value::String(Some("INV-001".to_owned())),);
}

#[test]
fn journal_line_cursor_invoice_id_none_is_null_string() {
    let mut model = sample_journal_line();
    model.invoice_id = None;
    let val =
        JournalLineODataMapper::extract_cursor_value(&model, JournalLineFilterField::InvoiceId);
    assert_eq!(val, sea_orm::Value::String(None));
}

// ---------------------------------------------------------------------------
// BalanceODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn balance_map_account_id() {
    assert_eq!(
        BalanceODataMapper::map_field(BalanceFilterField::AccountId).as_str(),
        BalanceColumn::AccountId.as_str(),
    );
}

#[test]
fn balance_map_account_class() {
    assert_eq!(
        BalanceODataMapper::map_field(BalanceFilterField::AccountClass).as_str(),
        BalanceColumn::AccountClass.as_str(),
    );
}

#[test]
fn balance_map_currency() {
    assert_eq!(
        BalanceODataMapper::map_field(BalanceFilterField::Currency).as_str(),
        BalanceColumn::Currency.as_str(),
    );
}

// ---------------------------------------------------------------------------
// BalanceODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn balance_cursor_account_id_is_uuid() {
    let model = sample_account_balance();
    let val = BalanceODataMapper::extract_cursor_value(&model, BalanceFilterField::AccountId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(Uuid::from_u128(0xB1))),);
}

#[test]
fn balance_cursor_account_class_is_string() {
    let model = sample_account_balance();
    let val = BalanceODataMapper::extract_cursor_value(&model, BalanceFilterField::AccountClass);
    assert_eq!(val, sea_orm::Value::String(Some("REVENUE".to_owned())),);
}

#[test]
fn balance_cursor_currency_is_string() {
    let model = sample_account_balance();
    let val = BalanceODataMapper::extract_cursor_value(&model, BalanceFilterField::Currency);
    assert_eq!(val, sea_orm::Value::String(Some("GBP".to_owned())),);
}

// ===========================================================================
// Fixtures for the mappers below.
//
// Each `sample_*` builds a full `Model` literal so a test can read one field
// without a DB — `map_field` and `extract_cursor_value` are both pure.
// Nullable columns are populated here and cleared in the dedicated NULL-arm
// tests, mirroring the `tenant_account` / `journal_line` pattern above.
// ===========================================================================

fn sample_journal_entry() -> journal_entry::Model {
    journal_entry::Model {
        entry_id: Uuid::from_u128(0x0C),
        tenant_id: Uuid::from_u128(0x1C),
        legal_entity_id: Uuid::from_u128(0x2C),
        period_id: "2025-03".to_owned(),
        entry_currency: "EUR".to_owned(),
        source_doc_type: "INVOICE_POST".to_owned(),
        source_business_id: "INV-2025-0007".to_owned(),
        reverses_entry_id: None,
        reverses_period_id: None,
        posted_at_utc: from_unix(1_700_000_000, 0).unwrap(),
        effective_at: NaiveDate::from_ymd_opt(2025, 3, 31).unwrap(),
        origin: "posting".to_owned(),
        posted_by_actor_id: Uuid::from_u128(0x3C),
        correlation_id: Uuid::from_u128(0x4C),
        rounding_evidence: JsonValue::Null,
        created_seq: 42,
        row_hash: None,
        prev_hash: None,
        prev_entry_id: None,
        prev_period_id: None,
    }
}

fn sample_refund() -> refund::Model {
    refund::Model {
        tenant_id: Uuid::from_u128(0x1D),
        refund_id: "RFND-001".to_owned(),
        psp_refund_id: "PSP-RFND-001".to_owned(),
        phase: "settled".to_owned(),
        pattern: "B".to_owned(),
        payment_id: "PAY-001".to_owned(),
        invoice_id: Some("INV-001".to_owned()),
        currency: "USD".to_owned(),
        amount_minor: 2_500,
        clearing_state: "cleared".to_owned(),
        relates_to_refund_id: None,
        reverses_entry_id: None,
        created_at_utc: from_unix(1_700_000_001, 0).unwrap(),
        version: 1,
    }
}

fn sample_credit_note() -> credit_note::Model {
    credit_note::Model {
        tenant_id: Uuid::from_u128(0x1E),
        credit_note_id: "CN-001".to_owned(),
        origin_invoice_id: "INV-002".to_owned(),
        origin_invoice_item_ref: None,
        revenue_stream: "SAAS".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 1_000,
        recognized_part_minor: 600,
        deferred_part_minor: 400,
        split_basis_ref: None,
        reason_code: "SERVICE_CREDIT".to_owned(),
        created_at_utc: from_unix(1_700_000_002, 0).unwrap(),
    }
}

fn sample_debit_note() -> debit_note::Model {
    debit_note::Model {
        tenant_id: Uuid::from_u128(0x1F),
        debit_note_id: "DN-001".to_owned(),
        origin_invoice_id: "INV-003".to_owned(),
        currency: "USD".to_owned(),
        amount_minor: 750,
        recognized_part_minor: 750,
        deferred_part_minor: 0,
        created_at_utc: from_unix(1_700_000_003, 0).unwrap(),
    }
}

fn sample_dispute() -> dispute::Model {
    dispute::Model {
        tenant_id: Uuid::from_u128(0x2D),
        dispute_id: "DSP-001".to_owned(),
        payment_id: "PAY-002".to_owned(),
        currency: "USD".to_owned(),
        variant: "chargeback".to_owned(),
        last_phase: "representment".to_owned(),
        cycle: 2,
        disputed_amount_minor: 5_000,
        cash_hold_minor: 5_000,
        version: 3,
    }
}

fn sample_recognition_run() -> recognition_run::Model {
    recognition_run::Model {
        tenant_id: Uuid::from_u128(0x2E),
        period_id: "2025-04".to_owned(),
        run_id: Uuid::from_u128(0x0E),
        started_at_utc: from_unix(1_700_000_004, 0).unwrap(),
        status: "completed".to_owned(),
    }
}

fn sample_exception() -> exception_queue::Model {
    exception_queue::Model {
        tenant_id: Uuid::from_u128(0x2F),
        exception_id: Uuid::from_u128(0x0F),
        exception_type: "UNAPPLIED_CASH".to_owned(),
        business_ref: "PAY-003".to_owned(),
        status: "open".to_owned(),
        period_id: Some("2025-05".to_owned()),
        detail: None,
        opened_at: from_unix(1_700_000_005, 0).unwrap(),
        resolved_at: None,
        resolved_by: None,
    }
}

// ---------------------------------------------------------------------------
// JournalEntryODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn journal_entry_map_entry_id() {
    assert_eq!(
        JournalEntryODataMapper::map_field(JournalEntryFilterField::EntryId).as_str(),
        JournalEntryColumn::EntryId.as_str()
    );
}

#[test]
fn journal_entry_map_source_doc_type() {
    assert_eq!(
        JournalEntryODataMapper::map_field(JournalEntryFilterField::SourceDocType).as_str(),
        JournalEntryColumn::SourceDocType.as_str()
    );
}

#[test]
fn journal_entry_map_source_business_id() {
    assert_eq!(
        JournalEntryODataMapper::map_field(JournalEntryFilterField::SourceBusinessId).as_str(),
        JournalEntryColumn::SourceBusinessId.as_str()
    );
}

#[test]
fn journal_entry_map_period_id() {
    assert_eq!(
        JournalEntryODataMapper::map_field(JournalEntryFilterField::PeriodId).as_str(),
        JournalEntryColumn::PeriodId.as_str()
    );
}

// ---------------------------------------------------------------------------
// JournalEntryODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn journal_entry_cursor_entry_id_is_uuid() {
    let model = sample_journal_entry();
    let val =
        JournalEntryODataMapper::extract_cursor_value(&model, JournalEntryFilterField::EntryId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(Uuid::from_u128(0x0C))));
}

#[test]
fn journal_entry_cursor_source_doc_type_is_string() {
    let model = sample_journal_entry();
    let val = JournalEntryODataMapper::extract_cursor_value(
        &model,
        JournalEntryFilterField::SourceDocType,
    );
    assert_eq!(val, sea_orm::Value::String(Some("INVOICE_POST".to_owned())));
}

#[test]
fn journal_entry_cursor_source_business_id_is_string() {
    let model = sample_journal_entry();
    let val = JournalEntryODataMapper::extract_cursor_value(
        &model,
        JournalEntryFilterField::SourceBusinessId,
    );
    assert_eq!(
        val,
        sea_orm::Value::String(Some("INV-2025-0007".to_owned()))
    );
}

#[test]
fn journal_entry_cursor_period_id_is_string() {
    let model = sample_journal_entry();
    let val =
        JournalEntryODataMapper::extract_cursor_value(&model, JournalEntryFilterField::PeriodId);
    assert_eq!(val, sea_orm::Value::String(Some("2025-03".to_owned())));
}

// ---------------------------------------------------------------------------
// RefundODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn refund_map_refund_id() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::RefundId).as_str(),
        RefundColumn::RefundId.as_str()
    );
}

#[test]
fn refund_map_payment_id() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::PaymentId).as_str(),
        RefundColumn::PaymentId.as_str()
    );
}

#[test]
fn refund_map_psp_refund_id() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::PspRefundId).as_str(),
        RefundColumn::PspRefundId.as_str()
    );
}

#[test]
fn refund_map_phase() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::Phase).as_str(),
        RefundColumn::Phase.as_str()
    );
}

#[test]
fn refund_map_pattern() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::Pattern).as_str(),
        RefundColumn::Pattern.as_str()
    );
}

#[test]
fn refund_map_clearing_state() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::ClearingState).as_str(),
        RefundColumn::ClearingState.as_str()
    );
}

#[test]
fn refund_map_invoice_id() {
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::InvoiceId).as_str(),
        RefundColumn::InvoiceId.as_str()
    );
}

// ---------------------------------------------------------------------------
// RefundODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn refund_cursor_refund_id_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::RefundId);
    assert_eq!(val, sea_orm::Value::String(Some("RFND-001".to_owned())));
}

#[test]
fn refund_cursor_payment_id_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::PaymentId);
    assert_eq!(val, sea_orm::Value::String(Some("PAY-001".to_owned())));
}

#[test]
fn refund_cursor_psp_refund_id_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::PspRefundId);
    assert_eq!(val, sea_orm::Value::String(Some("PSP-RFND-001".to_owned())));
}

#[test]
fn refund_cursor_phase_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::Phase);
    assert_eq!(val, sea_orm::Value::String(Some("settled".to_owned())));
}

#[test]
fn refund_cursor_pattern_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::Pattern);
    assert_eq!(val, sea_orm::Value::String(Some("B".to_owned())));
}

#[test]
fn refund_cursor_clearing_state_is_string() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::ClearingState);
    assert_eq!(val, sea_orm::Value::String(Some("cleared".to_owned())));
}

#[test]
fn refund_cursor_invoice_id_some() {
    let model = sample_refund();
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::InvoiceId);
    assert_eq!(val, sea_orm::Value::String(Some("INV-001".to_owned())));
}

/// The nullable-column arm: a `None` must become a *typed* NULL, not be skipped
/// or panic — the keyset predicate relies on the `Value` variant to stay
/// `String` so the comparison keeps its type.
#[test]
fn refund_cursor_invoice_id_none_is_null_string() {
    let mut model = sample_refund();
    model.invoice_id = None;
    let val = RefundODataMapper::extract_cursor_value(&model, RefundFilterField::InvoiceId);
    assert_eq!(val, sea_orm::Value::String(None));
}

// ---------------------------------------------------------------------------
// CreditNoteODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn credit_note_map_credit_note_id() {
    assert_eq!(
        CreditNoteODataMapper::map_field(CreditNoteFilterField::CreditNoteId).as_str(),
        CreditNoteColumn::CreditNoteId.as_str()
    );
}

#[test]
fn credit_note_map_origin_invoice_id() {
    assert_eq!(
        CreditNoteODataMapper::map_field(CreditNoteFilterField::OriginInvoiceId).as_str(),
        CreditNoteColumn::OriginInvoiceId.as_str()
    );
}

#[test]
fn credit_note_map_revenue_stream() {
    assert_eq!(
        CreditNoteODataMapper::map_field(CreditNoteFilterField::RevenueStream).as_str(),
        CreditNoteColumn::RevenueStream.as_str()
    );
}

#[test]
fn credit_note_map_reason_code() {
    assert_eq!(
        CreditNoteODataMapper::map_field(CreditNoteFilterField::ReasonCode).as_str(),
        CreditNoteColumn::ReasonCode.as_str()
    );
}

// ---------------------------------------------------------------------------
// CreditNoteODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn credit_note_cursor_credit_note_id_is_string() {
    let model = sample_credit_note();
    let val =
        CreditNoteODataMapper::extract_cursor_value(&model, CreditNoteFilterField::CreditNoteId);
    assert_eq!(val, sea_orm::Value::String(Some("CN-001".to_owned())));
}

#[test]
fn credit_note_cursor_origin_invoice_id_is_string() {
    let model = sample_credit_note();
    let val =
        CreditNoteODataMapper::extract_cursor_value(&model, CreditNoteFilterField::OriginInvoiceId);
    assert_eq!(val, sea_orm::Value::String(Some("INV-002".to_owned())));
}

/// `credit_note.revenue_stream` is NOT nullable, unlike `tenant_account`'s — so
/// this arm must emit `Some`, never the typed NULL.
#[test]
fn credit_note_cursor_revenue_stream_is_non_null_string() {
    let model = sample_credit_note();
    let val =
        CreditNoteODataMapper::extract_cursor_value(&model, CreditNoteFilterField::RevenueStream);
    assert_eq!(val, sea_orm::Value::String(Some("SAAS".to_owned())));
}

#[test]
fn credit_note_cursor_reason_code_is_string() {
    let model = sample_credit_note();
    let val =
        CreditNoteODataMapper::extract_cursor_value(&model, CreditNoteFilterField::ReasonCode);
    assert_eq!(
        val,
        sea_orm::Value::String(Some("SERVICE_CREDIT".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// DebitNoteODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn debit_note_map_debit_note_id() {
    assert_eq!(
        DebitNoteODataMapper::map_field(DebitNoteFilterField::DebitNoteId).as_str(),
        DebitNoteColumn::DebitNoteId.as_str()
    );
}

#[test]
fn debit_note_map_origin_invoice_id() {
    assert_eq!(
        DebitNoteODataMapper::map_field(DebitNoteFilterField::OriginInvoiceId).as_str(),
        DebitNoteColumn::OriginInvoiceId.as_str()
    );
}

// ---------------------------------------------------------------------------
// DebitNoteODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn debit_note_cursor_debit_note_id_is_string() {
    let model = sample_debit_note();
    let val = DebitNoteODataMapper::extract_cursor_value(&model, DebitNoteFilterField::DebitNoteId);
    assert_eq!(val, sea_orm::Value::String(Some("DN-001".to_owned())));
}

#[test]
fn debit_note_cursor_origin_invoice_id_is_string() {
    let model = sample_debit_note();
    let val =
        DebitNoteODataMapper::extract_cursor_value(&model, DebitNoteFilterField::OriginInvoiceId);
    assert_eq!(val, sea_orm::Value::String(Some("INV-003".to_owned())));
}

// ---------------------------------------------------------------------------
// DisputeODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn dispute_map_dispute_id() {
    assert_eq!(
        DisputeODataMapper::map_field(DisputeFilterField::DisputeId).as_str(),
        DisputeColumn::DisputeId.as_str()
    );
}

#[test]
fn dispute_map_payment_id() {
    assert_eq!(
        DisputeODataMapper::map_field(DisputeFilterField::PaymentId).as_str(),
        DisputeColumn::PaymentId.as_str()
    );
}

#[test]
fn dispute_map_last_phase() {
    assert_eq!(
        DisputeODataMapper::map_field(DisputeFilterField::LastPhase).as_str(),
        DisputeColumn::LastPhase.as_str()
    );
}

#[test]
fn dispute_map_variant() {
    assert_eq!(
        DisputeODataMapper::map_field(DisputeFilterField::Variant).as_str(),
        DisputeColumn::Variant.as_str()
    );
}

// ---------------------------------------------------------------------------
// DisputeODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn dispute_cursor_dispute_id_is_string() {
    let model = sample_dispute();
    let val = DisputeODataMapper::extract_cursor_value(&model, DisputeFilterField::DisputeId);
    assert_eq!(val, sea_orm::Value::String(Some("DSP-001".to_owned())));
}

#[test]
fn dispute_cursor_payment_id_is_string() {
    let model = sample_dispute();
    let val = DisputeODataMapper::extract_cursor_value(&model, DisputeFilterField::PaymentId);
    assert_eq!(val, sea_orm::Value::String(Some("PAY-002".to_owned())));
}

#[test]
fn dispute_cursor_last_phase_is_string() {
    let model = sample_dispute();
    let val = DisputeODataMapper::extract_cursor_value(&model, DisputeFilterField::LastPhase);
    assert_eq!(
        val,
        sea_orm::Value::String(Some("representment".to_owned()))
    );
}

/// `variant` must map to the `variant` column, not `last_phase` — the two are
/// adjacent `String` fields, so a copy-paste slip here would silently order the
/// keyset by the wrong column.
#[test]
fn dispute_cursor_variant_is_string() {
    let model = sample_dispute();
    let val = DisputeODataMapper::extract_cursor_value(&model, DisputeFilterField::Variant);
    assert_eq!(val, sea_orm::Value::String(Some("chargeback".to_owned())));
}

// ---------------------------------------------------------------------------
// RecognitionRunODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

#[test]
fn recognition_run_map_run_id() {
    assert_eq!(
        RecognitionRunODataMapper::map_field(RecognitionRunFilterField::RunId).as_str(),
        RecognitionRunColumn::RunId.as_str()
    );
}

#[test]
fn recognition_run_map_period_id() {
    assert_eq!(
        RecognitionRunODataMapper::map_field(RecognitionRunFilterField::PeriodId).as_str(),
        RecognitionRunColumn::PeriodId.as_str()
    );
}

#[test]
fn recognition_run_map_status() {
    assert_eq!(
        RecognitionRunODataMapper::map_field(RecognitionRunFilterField::Status).as_str(),
        RecognitionRunColumn::Status.as_str()
    );
}

// ---------------------------------------------------------------------------
// RecognitionRunODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn recognition_run_cursor_run_id_is_uuid() {
    let model = sample_recognition_run();
    let val =
        RecognitionRunODataMapper::extract_cursor_value(&model, RecognitionRunFilterField::RunId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(Uuid::from_u128(0x0E))));
}

#[test]
fn recognition_run_cursor_period_id_is_string() {
    let model = sample_recognition_run();
    let val = RecognitionRunODataMapper::extract_cursor_value(
        &model,
        RecognitionRunFilterField::PeriodId,
    );
    assert_eq!(val, sea_orm::Value::String(Some("2025-04".to_owned())));
}

#[test]
fn recognition_run_cursor_status_is_string() {
    let model = sample_recognition_run();
    let val =
        RecognitionRunODataMapper::extract_cursor_value(&model, RecognitionRunFilterField::Status);
    assert_eq!(val, sea_orm::Value::String(Some("completed".to_owned())));
}

// ---------------------------------------------------------------------------
// ExceptionODataMapper — map_field (column name lock)
// ---------------------------------------------------------------------------

/// `exception_type` is the Rust field name; the SQL column is `type` (a
/// reserved word), so this lock is load-bearing.
#[test]
fn exception_map_exception_type() {
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::ExceptionType).as_str(),
        ExceptionColumn::ExceptionType.as_str()
    );
}

#[test]
fn exception_map_exception_id() {
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::ExceptionId).as_str(),
        ExceptionColumn::ExceptionId.as_str()
    );
}

#[test]
fn exception_map_status() {
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::Status).as_str(),
        ExceptionColumn::Status.as_str()
    );
}

#[test]
fn exception_map_business_ref() {
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::BusinessRef).as_str(),
        ExceptionColumn::BusinessRef.as_str()
    );
}

#[test]
fn exception_map_period_id() {
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::PeriodId).as_str(),
        ExceptionColumn::PeriodId.as_str()
    );
}

// ---------------------------------------------------------------------------
// ExceptionODataMapper — extract_cursor_value
// ---------------------------------------------------------------------------

#[test]
fn exception_cursor_exception_id_is_uuid() {
    let model = sample_exception();
    let val = ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::ExceptionId);
    assert_eq!(val, sea_orm::Value::Uuid(Some(Uuid::from_u128(0x0F))));
}

#[test]
fn exception_cursor_exception_type_is_string() {
    let model = sample_exception();
    let val =
        ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::ExceptionType);
    assert_eq!(
        val,
        sea_orm::Value::String(Some("UNAPPLIED_CASH".to_owned()))
    );
}

#[test]
fn exception_cursor_status_is_string() {
    let model = sample_exception();
    let val = ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::Status);
    assert_eq!(val, sea_orm::Value::String(Some("open".to_owned())));
}

#[test]
fn exception_cursor_business_ref_is_string() {
    let model = sample_exception();
    let val = ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::BusinessRef);
    assert_eq!(val, sea_orm::Value::String(Some("PAY-003".to_owned())));
}

#[test]
fn exception_cursor_period_id_some() {
    let model = sample_exception();
    let val = ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::PeriodId);
    assert_eq!(val, sea_orm::Value::String(Some("2025-05".to_owned())));
}

/// The second nullable-column arm: a non-period exception has no `period_id`,
/// which must still produce a typed NULL rather than being dropped.
#[test]
fn exception_cursor_period_id_none_is_null_string() {
    let mut model = sample_exception();
    model.period_id = None;
    let val = ExceptionODataMapper::extract_cursor_value(&model, ExceptionFilterField::PeriodId);
    assert_eq!(val, sea_orm::Value::String(None));
}

// A nullable column cannot carry a keyset walk: the mapper extracts
// `Value::String(None)` and the cursor codec has no arm for it, so a NULL on a
// page boundary would answer 400 to a request the OpenAPI document declares
// valid. Each of the four is refused as an order key while staying filterable.
#[test]
fn nullable_columns_are_refused_as_order_keys() {
    assert!(!AccountInfoODataMapper::is_orderable(
        AccountInfoFilterField::RevenueStream
    ));
    assert!(!JournalLineODataMapper::is_orderable(
        JournalLineFilterField::InvoiceId
    ));
    assert!(!RefundODataMapper::is_orderable(
        RefundFilterField::InvoiceId
    ));
    assert!(!ExceptionODataMapper::is_orderable(
        ExceptionFilterField::PeriodId
    ));
}

// The refusal is scoped to the nullable column: every mapper that got an
// override must still order on the key its walk actually uses as a tiebreaker,
// which goes through the same gate.
#[test]
fn the_walks_own_tiebreakers_stay_orderable() {
    assert!(AccountInfoODataMapper::is_orderable(
        AccountInfoFilterField::AccountId
    ));
    assert!(JournalLineODataMapper::is_orderable(
        JournalLineFilterField::LineId
    ));
    assert!(RefundODataMapper::is_orderable(RefundFilterField::RefundId));
    assert!(ExceptionODataMapper::is_orderable(
        ExceptionFilterField::ExceptionId
    ));
}

// ---------------------------------------------------------------------------
// The tenant axis, across all ten mappers
// ---------------------------------------------------------------------------

// `TenantId` is the one filter field every collection shares, and it is the
// scoping axis: it must resolve to the row's own owning-tenant column on each
// table. `journal_line` is the trap — it carries four tenant columns
// (`tenant_id`, `payer_tenant_id`, `seller_tenant_id`, `resource_tenant_id`),
// so a mapper that routed the shared field at a role column would scope a
// listing by the wrong party and still typecheck.
#[test]
fn every_mapper_routes_the_tenant_field_at_its_own_tenant_column() {
    assert_eq!(
        AccountInfoODataMapper::map_field(AccountInfoFilterField::TenantId).as_str(),
        TenantAccountColumn::TenantId.as_str(),
    );
    assert_eq!(
        JournalLineODataMapper::map_field(JournalLineFilterField::TenantId).as_str(),
        JournalLineColumn::TenantId.as_str(),
    );
    assert_eq!(
        JournalEntryODataMapper::map_field(JournalEntryFilterField::TenantId).as_str(),
        JournalEntryColumn::TenantId.as_str(),
    );
    assert_eq!(
        BalanceODataMapper::map_field(BalanceFilterField::TenantId).as_str(),
        BalanceColumn::TenantId.as_str(),
    );
    assert_eq!(
        RefundODataMapper::map_field(RefundFilterField::TenantId).as_str(),
        RefundColumn::TenantId.as_str(),
    );
    assert_eq!(
        CreditNoteODataMapper::map_field(CreditNoteFilterField::TenantId).as_str(),
        CreditNoteColumn::TenantId.as_str(),
    );
    assert_eq!(
        DebitNoteODataMapper::map_field(DebitNoteFilterField::TenantId).as_str(),
        DebitNoteColumn::TenantId.as_str(),
    );
    assert_eq!(
        DisputeODataMapper::map_field(DisputeFilterField::TenantId).as_str(),
        DisputeColumn::TenantId.as_str(),
    );
    assert_eq!(
        RecognitionRunODataMapper::map_field(RecognitionRunFilterField::TenantId).as_str(),
        RecognitionRunColumn::TenantId.as_str(),
    );
    assert_eq!(
        ExceptionODataMapper::map_field(ExceptionFilterField::TenantId).as_str(),
        ExceptionColumn::TenantId.as_str(),
    );
}

/// The `journal_line` trap spelled out: the shared scoping field and the
/// payer-role field are different columns, so the two must not collapse.
#[test]
fn the_journal_line_tenant_field_is_not_the_payer_tenant_field() {
    assert_ne!(
        JournalLineODataMapper::map_field(JournalLineFilterField::TenantId).as_str(),
        JournalLineODataMapper::map_field(JournalLineFilterField::PayerTenantId).as_str(),
    );
}

// The cursor half of the same axis. `tenant_id` is a `Uuid` column on every
// table, so it must extract as `Value::Uuid` — a `Value::String` would compare
// against a `uuid` column as text and the keyset walk would fail in the engine,
// not here. Each fixture carries a distinct tenant so a mapper reading some
// other row field cannot pass by coincidence.
#[test]
fn every_mapper_extracts_the_tenant_cursor_as_a_uuid() {
    let account = sample_tenant_account();
    assert_eq!(
        AccountInfoODataMapper::extract_cursor_value(&account, AccountInfoFilterField::TenantId),
        sea_orm::Value::Uuid(Some(account.tenant_id)),
    );

    let line = sample_journal_line();
    assert_eq!(
        JournalLineODataMapper::extract_cursor_value(&line, JournalLineFilterField::TenantId),
        sea_orm::Value::Uuid(Some(line.tenant_id)),
    );

    let entry = sample_journal_entry();
    assert_eq!(
        JournalEntryODataMapper::extract_cursor_value(&entry, JournalEntryFilterField::TenantId),
        sea_orm::Value::Uuid(Some(entry.tenant_id)),
    );

    let balance = sample_account_balance();
    assert_eq!(
        BalanceODataMapper::extract_cursor_value(&balance, BalanceFilterField::TenantId),
        sea_orm::Value::Uuid(Some(balance.tenant_id)),
    );

    let refund = sample_refund();
    assert_eq!(
        RefundODataMapper::extract_cursor_value(&refund, RefundFilterField::TenantId),
        sea_orm::Value::Uuid(Some(refund.tenant_id)),
    );

    let credit_note = sample_credit_note();
    assert_eq!(
        CreditNoteODataMapper::extract_cursor_value(&credit_note, CreditNoteFilterField::TenantId),
        sea_orm::Value::Uuid(Some(credit_note.tenant_id)),
    );

    let debit_note = sample_debit_note();
    assert_eq!(
        DebitNoteODataMapper::extract_cursor_value(&debit_note, DebitNoteFilterField::TenantId),
        sea_orm::Value::Uuid(Some(debit_note.tenant_id)),
    );

    let dispute = sample_dispute();
    assert_eq!(
        DisputeODataMapper::extract_cursor_value(&dispute, DisputeFilterField::TenantId),
        sea_orm::Value::Uuid(Some(dispute.tenant_id)),
    );

    let run = sample_recognition_run();
    assert_eq!(
        RecognitionRunODataMapper::extract_cursor_value(&run, RecognitionRunFilterField::TenantId),
        sea_orm::Value::Uuid(Some(run.tenant_id)),
    );

    let exception = sample_exception();
    assert_eq!(
        ExceptionODataMapper::extract_cursor_value(&exception, ExceptionFilterField::TenantId),
        sea_orm::Value::Uuid(Some(exception.tenant_id)),
    );
}

// The ten fixtures each carry a distinct tenant, which is what makes the
// extraction test above discriminating: a mapper that read a neighbouring
// `Uuid` field would have to match this row's tenant by accident.
#[test]
fn the_fixtures_carry_ten_distinct_tenants() {
    let tenants = [
        sample_tenant_account().tenant_id,
        sample_journal_line().tenant_id,
        sample_journal_entry().tenant_id,
        sample_account_balance().tenant_id,
        sample_refund().tenant_id,
        sample_credit_note().tenant_id,
        sample_debit_note().tenant_id,
        sample_dispute().tenant_id,
        sample_recognition_run().tenant_id,
        sample_exception().tenant_id,
    ];
    let mut unique = tenants.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), tenants.len());
}
