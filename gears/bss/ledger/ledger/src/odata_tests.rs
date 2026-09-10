//! Unit tests for the ledger's `OData` filter-field enums: wire names, field
//! kinds, and (critically) that each enum's default keyset-order column
//! resolves via [`FilterField::from_name`] so the repo's default-order
//! injection cannot error "Unknown orderby field".

use super::{
    AccountInfoFilterField, BalanceFilterField, CreditNoteFilterField, DebitNoteFilterField,
    DisputeFilterField, ExceptionFilterField, JournalEntryFilterField, JournalLineFilterField,
    RecognitionRunFilterField, RefundFilterField,
};
use toolkit_odata::filter::{FieldKind, FilterField};

#[test]
fn account_fields_expose_expected_wire_names_and_kinds() {
    assert_eq!(AccountInfoFilterField::TenantId.name(), "tenant_id");
    assert_eq!(AccountInfoFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(AccountInfoFilterField::AccountId.name(), "account_id");
    assert_eq!(AccountInfoFilterField::AccountClass.name(), "account_class");
    assert_eq!(AccountInfoFilterField::Currency.name(), "currency");
    assert_eq!(
        AccountInfoFilterField::RevenueStream.name(),
        "revenue_stream"
    );
    assert_eq!(
        AccountInfoFilterField::LifecycleState.name(),
        "lifecycle_state"
    );
    assert_eq!(AccountInfoFilterField::AccountId.kind(), FieldKind::Uuid);
    assert_eq!(
        AccountInfoFilterField::AccountClass.kind(),
        FieldKind::String
    );
}

#[test]
fn line_fields_expose_expected_wire_names_and_kinds() {
    assert_eq!(JournalLineFilterField::TenantId.name(), "tenant_id");
    assert_eq!(JournalLineFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(JournalLineFilterField::LineId.name(), "line_id");
    assert_eq!(
        JournalLineFilterField::PayerTenantId.name(),
        "payer_tenant_id"
    );
    assert_eq!(JournalLineFilterField::AccountClass.name(), "account_class");
    assert_eq!(JournalLineFilterField::PeriodId.name(), "period_id");
    assert_eq!(JournalLineFilterField::InvoiceId.name(), "invoice_id");
    assert_eq!(JournalLineFilterField::LineId.kind(), FieldKind::Uuid);
    assert_eq!(
        JournalLineFilterField::PayerTenantId.kind(),
        FieldKind::Uuid
    );
    assert_eq!(JournalLineFilterField::PeriodId.kind(), FieldKind::String);
}

#[test]
fn balance_fields_expose_expected_wire_names_and_kinds() {
    assert_eq!(BalanceFilterField::TenantId.name(), "tenant_id");
    assert_eq!(BalanceFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(BalanceFilterField::AccountId.name(), "account_id");
    assert_eq!(BalanceFilterField::AccountClass.name(), "account_class");
    assert_eq!(BalanceFilterField::Currency.name(), "currency");
    assert_eq!(BalanceFilterField::AccountId.kind(), FieldKind::Uuid);
    assert_eq!(BalanceFilterField::Currency.kind(), FieldKind::String);
}

/// The repo injects a default `(<key> ASC)` order on a bare list. That key
/// MUST resolve back to a variant via `from_name`, else `paginate_odata` errors
/// "Unknown orderby field" — the RBAC C1 regression. Guard each enum's default
/// keyset column.
#[test]
fn default_order_columns_resolve_via_from_name() {
    assert_eq!(
        AccountInfoFilterField::from_name("account_id"),
        Some(AccountInfoFilterField::AccountId)
    );
    assert_eq!(
        JournalLineFilterField::from_name("line_id"),
        Some(JournalLineFilterField::LineId)
    );
    assert_eq!(
        BalanceFilterField::from_name("account_id"),
        Some(BalanceFilterField::AccountId)
    );
}

#[test]
fn unknown_field_does_not_resolve() {
    assert_eq!(AccountInfoFilterField::from_name("nope"), None);
    assert_eq!(
        JournalLineFilterField::from_name("source_business_id"),
        None
    );
    assert_eq!(BalanceFilterField::from_name("balance_minor"), None);
}

// ── New read-surface filter enums (refund / notes / dispute / recognition-run /
// journal-entry header). Each test pins EVERY variant's wire name + kind, asserts
// `FIELDS` lists every variant, and round-trips `from_name(name())` so the repo's
// default-order injection (and any `$orderby` on a listed dim) resolves. ─────────

#[test]
fn refund_fields_round_trips() {
    assert_eq!(RefundFilterField::TenantId.name(), "tenant_id");
    assert_eq!(RefundFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(RefundFilterField::RefundId.name(), "refund_id");
    assert_eq!(RefundFilterField::PaymentId.name(), "payment_id");
    assert_eq!(RefundFilterField::PspRefundId.name(), "psp_refund_id");
    assert_eq!(RefundFilterField::Phase.name(), "phase");
    assert_eq!(RefundFilterField::Pattern.name(), "pattern");
    assert_eq!(RefundFilterField::ClearingState.name(), "clearing_state");
    assert_eq!(RefundFilterField::InvoiceId.name(), "invoice_id");

    // Seller scope is Uuid; every other refund filter dim is a `varchar`.
    for f in RefundFilterField::FIELDS {
        if *f == RefundFilterField::TenantId {
            assert_eq!(f.kind(), FieldKind::Uuid, "{f:?} must be Uuid");
        } else {
            assert_eq!(f.kind(), FieldKind::String, "{f:?} must be String");
        }
    }

    assert_eq!(
        RefundFilterField::FIELDS,
        &[
            RefundFilterField::TenantId,
            RefundFilterField::RefundId,
            RefundFilterField::PaymentId,
            RefundFilterField::PspRefundId,
            RefundFilterField::Phase,
            RefundFilterField::Pattern,
            RefundFilterField::ClearingState,
            RefundFilterField::InvoiceId,
        ],
        "FIELDS must list every variant"
    );

    // The default keyset-order column resolves, and so does every listed dim.
    assert_eq!(
        RefundFilterField::from_name("refund_id"),
        Some(RefundFilterField::RefundId)
    );
    for f in RefundFilterField::FIELDS {
        assert_eq!(
            RefundFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn credit_note_fields_round_trips() {
    assert_eq!(CreditNoteFilterField::TenantId.name(), "tenant_id");
    assert_eq!(CreditNoteFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(CreditNoteFilterField::CreditNoteId.name(), "credit_note_id");
    assert_eq!(
        CreditNoteFilterField::OriginInvoiceId.name(),
        "origin_invoice_id"
    );
    assert_eq!(
        CreditNoteFilterField::RevenueStream.name(),
        "revenue_stream"
    );
    assert_eq!(CreditNoteFilterField::ReasonCode.name(), "reason_code");

    for f in CreditNoteFilterField::FIELDS {
        if *f == CreditNoteFilterField::TenantId {
            assert_eq!(f.kind(), FieldKind::Uuid, "{f:?} must be Uuid");
        } else {
            assert_eq!(f.kind(), FieldKind::String, "{f:?} must be String");
        }
    }

    assert_eq!(
        CreditNoteFilterField::FIELDS,
        &[
            CreditNoteFilterField::TenantId,
            CreditNoteFilterField::CreditNoteId,
            CreditNoteFilterField::OriginInvoiceId,
            CreditNoteFilterField::RevenueStream,
            CreditNoteFilterField::ReasonCode,
        ],
        "FIELDS must list every variant"
    );

    assert_eq!(
        CreditNoteFilterField::from_name("credit_note_id"),
        Some(CreditNoteFilterField::CreditNoteId)
    );
    for f in CreditNoteFilterField::FIELDS {
        assert_eq!(
            CreditNoteFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn debit_note_fields_round_trips() {
    assert_eq!(DebitNoteFilterField::TenantId.name(), "tenant_id");
    assert_eq!(DebitNoteFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(DebitNoteFilterField::DebitNoteId.name(), "debit_note_id");
    assert_eq!(
        DebitNoteFilterField::OriginInvoiceId.name(),
        "origin_invoice_id"
    );

    for f in DebitNoteFilterField::FIELDS {
        if *f == DebitNoteFilterField::TenantId {
            assert_eq!(f.kind(), FieldKind::Uuid, "{f:?} must be Uuid");
        } else {
            assert_eq!(f.kind(), FieldKind::String, "{f:?} must be String");
        }
    }

    assert_eq!(
        DebitNoteFilterField::FIELDS,
        &[
            DebitNoteFilterField::TenantId,
            DebitNoteFilterField::DebitNoteId,
            DebitNoteFilterField::OriginInvoiceId,
        ],
        "FIELDS must list every variant"
    );

    assert_eq!(
        DebitNoteFilterField::from_name("debit_note_id"),
        Some(DebitNoteFilterField::DebitNoteId)
    );
    for f in DebitNoteFilterField::FIELDS {
        assert_eq!(
            DebitNoteFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn dispute_fields_round_trips() {
    assert_eq!(DisputeFilterField::TenantId.name(), "tenant_id");
    assert_eq!(DisputeFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(DisputeFilterField::DisputeId.name(), "dispute_id");
    assert_eq!(DisputeFilterField::PaymentId.name(), "payment_id");
    assert_eq!(DisputeFilterField::LastPhase.name(), "last_phase");
    assert_eq!(DisputeFilterField::Variant.name(), "variant");

    for f in DisputeFilterField::FIELDS {
        if *f == DisputeFilterField::TenantId {
            assert_eq!(f.kind(), FieldKind::Uuid, "{f:?} must be Uuid");
        } else {
            assert_eq!(f.kind(), FieldKind::String, "{f:?} must be String");
        }
    }

    assert_eq!(
        DisputeFilterField::FIELDS,
        &[
            DisputeFilterField::TenantId,
            DisputeFilterField::DisputeId,
            DisputeFilterField::PaymentId,
            DisputeFilterField::LastPhase,
            DisputeFilterField::Variant,
        ],
        "FIELDS must list every variant"
    );

    assert_eq!(
        DisputeFilterField::from_name("dispute_id"),
        Some(DisputeFilterField::DisputeId)
    );
    for f in DisputeFilterField::FIELDS {
        assert_eq!(
            DisputeFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn recognition_run_fields_round_trips() {
    assert_eq!(RecognitionRunFilterField::TenantId.name(), "tenant_id");
    assert_eq!(RecognitionRunFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(RecognitionRunFilterField::RunId.name(), "run_id");
    assert_eq!(RecognitionRunFilterField::PeriodId.name(), "period_id");
    assert_eq!(RecognitionRunFilterField::Status.name(), "status");

    // `run_id` is the surrogate `Uuid` PK leg; `period_id` / `status` are `varchar`.
    assert_eq!(RecognitionRunFilterField::RunId.kind(), FieldKind::Uuid);
    assert_eq!(
        RecognitionRunFilterField::PeriodId.kind(),
        FieldKind::String
    );
    assert_eq!(RecognitionRunFilterField::Status.kind(), FieldKind::String);

    assert_eq!(
        RecognitionRunFilterField::FIELDS,
        &[
            RecognitionRunFilterField::TenantId,
            RecognitionRunFilterField::RunId,
            RecognitionRunFilterField::PeriodId,
            RecognitionRunFilterField::Status,
        ],
        "FIELDS must list every variant"
    );

    assert_eq!(
        RecognitionRunFilterField::from_name("run_id"),
        Some(RecognitionRunFilterField::RunId)
    );
    for f in RecognitionRunFilterField::FIELDS {
        assert_eq!(
            RecognitionRunFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn journal_entry_fields_round_trips() {
    assert_eq!(JournalEntryFilterField::TenantId.name(), "tenant_id");
    assert_eq!(JournalEntryFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(JournalEntryFilterField::EntryId.name(), "entry_id");
    assert_eq!(
        JournalEntryFilterField::SourceDocType.name(),
        "source_doc_type"
    );
    assert_eq!(
        JournalEntryFilterField::SourceBusinessId.name(),
        "source_business_id"
    );
    assert_eq!(JournalEntryFilterField::PeriodId.name(), "period_id");

    // `entry_id` is the `Uuid` keyset leg; the header dims are `varchar`.
    assert_eq!(JournalEntryFilterField::EntryId.kind(), FieldKind::Uuid);
    assert_eq!(
        JournalEntryFilterField::SourceDocType.kind(),
        FieldKind::String
    );
    assert_eq!(
        JournalEntryFilterField::SourceBusinessId.kind(),
        FieldKind::String
    );
    assert_eq!(JournalEntryFilterField::PeriodId.kind(), FieldKind::String);

    assert_eq!(
        JournalEntryFilterField::FIELDS,
        &[
            JournalEntryFilterField::TenantId,
            JournalEntryFilterField::EntryId,
            JournalEntryFilterField::SourceDocType,
            JournalEntryFilterField::SourceBusinessId,
            JournalEntryFilterField::PeriodId,
        ],
        "FIELDS must list every variant"
    );

    assert_eq!(
        JournalEntryFilterField::from_name("entry_id"),
        Some(JournalEntryFilterField::EntryId)
    );
    for f in JournalEntryFilterField::FIELDS {
        assert_eq!(
            JournalEntryFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}

#[test]
fn exception_fields_round_trips() {
    assert_eq!(ExceptionFilterField::TenantId.name(), "tenant_id");
    assert_eq!(ExceptionFilterField::ExceptionId.name(), "exception_id");
    assert_eq!(ExceptionFilterField::ExceptionType.name(), "type");
    assert_eq!(ExceptionFilterField::Status.name(), "status");
    assert_eq!(ExceptionFilterField::BusinessRef.name(), "business_ref");
    assert_eq!(ExceptionFilterField::PeriodId.name(), "period_id");
    assert_eq!(ExceptionFilterField::TenantId.kind(), FieldKind::Uuid);
    assert_eq!(
        ExceptionFilterField::ExceptionType.kind(),
        FieldKind::String
    );
    assert_eq!(
        ExceptionFilterField::from_name("type"),
        Some(ExceptionFilterField::ExceptionType)
    );
    for f in ExceptionFilterField::FIELDS {
        assert_eq!(
            ExceptionFilterField::from_name(f.name()),
            Some(*f),
            "{f:?} must round-trip via from_name(name())"
        );
    }
}
