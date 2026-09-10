//! `OData` filter-field schemas for the ledger's collection GETs.
//!
//! Dummy structs: the user-facing request shape is `ODataQuery`. The derive
//! generates `{Name}FilterField`, re-exported under the names the gear already
//! uses. `tenant_id` is a seller-scope filter on every in-scope list.

// `DebitNoteQuery`'s filter keys are all ids, so the `FilterField` enum the
// derive builds trips `enum_variant_names`. The names ARE the OData wire
// contract; the suppression stays in the one SDK that has such a collection
// rather than in the macro, which would silence it for every gear.
#![allow(clippy::enum_variant_names)]

use toolkit_odata::filter::{FieldKind, FilterField};
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct AccountInfoQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub account_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub account_class: String,
    #[odata(filter(kind = "String"))]
    pub currency: String,
    #[odata(filter(kind = "String"))]
    pub revenue_stream: String,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct JournalLineQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub line_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub payer_tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub account_class: String,
    #[odata(filter(kind = "String"))]
    pub period_id: String,
    #[odata(filter(kind = "String"))]
    pub invoice_id: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct JournalEntryQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub entry_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub source_doc_type: String,
    #[odata(filter(kind = "String"))]
    pub source_business_id: String,
    #[odata(filter(kind = "String"))]
    pub period_id: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct BalanceQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub account_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub account_class: String,
    #[odata(filter(kind = "String"))]
    pub currency: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct RefundQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub refund_id: String,
    #[odata(filter(kind = "String"))]
    pub payment_id: String,
    #[odata(filter(kind = "String"))]
    pub psp_refund_id: String,
    #[odata(filter(kind = "String"))]
    pub phase: String,
    #[odata(filter(kind = "String"))]
    pub pattern: String,
    #[odata(filter(kind = "String"))]
    pub clearing_state: String,
    #[odata(filter(kind = "String"))]
    pub invoice_id: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct CreditNoteQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub credit_note_id: String,
    #[odata(filter(kind = "String"))]
    pub origin_invoice_id: String,
    #[odata(filter(kind = "String"))]
    pub revenue_stream: String,
    #[odata(filter(kind = "String"))]
    pub reason_code: String,
}

#[derive(ODataFilterable)]
// Every filter key this collection accepts is an id. The field names ARE the
// OData wire contract — renaming them to satisfy the postfix lint would rename
// the query keys callers send. The `FilterField` enum the derive builds trips
// `enum_variant_names` for the same reason; the module-level allow at the top
// of this file covers it, deliberately not the macro.
#[allow(dead_code, clippy::struct_field_names)]
struct DebitNoteQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub debit_note_id: String,
    #[odata(filter(kind = "String"))]
    pub origin_invoice_id: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct DisputeQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub dispute_id: String,
    #[odata(filter(kind = "String"))]
    pub payment_id: String,
    #[odata(filter(kind = "String"))]
    pub last_phase: String,
    #[odata(filter(kind = "String"))]
    pub variant: String,
}

#[derive(ODataFilterable)]
#[allow(dead_code)]
struct RecognitionRunQuery {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub run_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub period_id: String,
    #[odata(filter(kind = "String"))]
    pub status: String,
}

/// Sortable fields, narrower than the query enum above.
///
/// The mapper's `is_orderable` refuses the field left out — it has no index to
/// seek, or is nullable, so a keyset page would scan and sort the whole table —
/// and declaring `$orderby` from the *filter* enum published it in
/// `docs/api/api.json` anyway. A generated client offering a sort the server
/// answers 400 for is a contract that is wrong in the direction nobody checks.
#[derive(ODataFilterable)]
#[allow(dead_code)]
struct AccountInfoOrder {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub account_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub account_class: String,
    #[odata(filter(kind = "String"))]
    pub currency: String,
    #[odata(filter(kind = "String"))]
    pub lifecycle_state: String,
}

/// Sortable fields, narrower than the query enum above.
///
/// The mapper's `is_orderable` refuses the field left out — it has no index to
/// seek, or is nullable, so a keyset page would scan and sort the whole table —
/// and declaring `$orderby` from the *filter* enum published it in
/// `docs/api/api.json` anyway. A generated client offering a sort the server
/// answers 400 for is a contract that is wrong in the direction nobody checks.
#[derive(ODataFilterable)]
#[allow(dead_code)]
struct JournalLineOrder {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub line_id: Uuid,
    #[odata(filter(kind = "Uuid"))]
    pub payer_tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub account_class: String,
    #[odata(filter(kind = "String"))]
    pub period_id: String,
}

/// Sortable fields, narrower than the query enum above.
///
/// The mapper's `is_orderable` refuses the field left out — it has no index to
/// seek, or is nullable, so a keyset page would scan and sort the whole table —
/// and declaring `$orderby` from the *filter* enum published it in
/// `docs/api/api.json` anyway. A generated client offering a sort the server
/// answers 400 for is a contract that is wrong in the direction nobody checks.
#[derive(ODataFilterable)]
#[allow(dead_code)]
struct RefundOrder {
    #[odata(filter(kind = "Uuid"))]
    pub tenant_id: Uuid,
    #[odata(filter(kind = "String"))]
    pub refund_id: String,
    #[odata(filter(kind = "String"))]
    pub payment_id: String,
    #[odata(filter(kind = "String"))]
    pub psp_refund_id: String,
    #[odata(filter(kind = "String"))]
    pub phase: String,
    #[odata(filter(kind = "String"))]
    pub pattern: String,
    #[odata(filter(kind = "String"))]
    pub clearing_state: String,
}

/// Hand-written: the wire field is `type`, a Rust keyword. The derive turns
/// `r#type` into variant `RType` and name `r#type`, which would break
/// `$filter=type eq '…'`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ExceptionFilterField {
    TenantId,
    ExceptionId,
    /// Wire name `type`.
    ExceptionType,
    Status,
    BusinessRef,
    PeriodId,
}

impl FilterField for ExceptionFilterField {
    const FIELDS: &'static [Self] = &[
        Self::TenantId,
        Self::ExceptionId,
        Self::ExceptionType,
        Self::Status,
        Self::BusinessRef,
        Self::PeriodId,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::TenantId => "tenant_id",
            Self::ExceptionId => "exception_id",
            Self::ExceptionType => "type",
            Self::Status => "status",
            Self::BusinessRef => "business_ref",
            Self::PeriodId => "period_id",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::TenantId | Self::ExceptionId => FieldKind::Uuid,
            Self::ExceptionType | Self::Status | Self::BusinessRef | Self::PeriodId => {
                FieldKind::String
            }
        }
    }
}

/// Sortable fields of the exception list, narrower than [`ExceptionFilterField`]
/// for the reason the derived `*Order` structs above carry: `period_id` is
/// refused by `ExceptionODataMapper::is_orderable`. Hand-written for the same
/// reason its filter twin is.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ExceptionOrderField {
    TenantId,
    ExceptionId,
    /// Wire name `type`.
    ExceptionType,
    Status,
    BusinessRef,
}

impl FilterField for ExceptionOrderField {
    const FIELDS: &'static [Self] = &[
        Self::TenantId,
        Self::ExceptionId,
        Self::ExceptionType,
        Self::Status,
        Self::BusinessRef,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::TenantId => "tenant_id",
            Self::ExceptionId => "exception_id",
            Self::ExceptionType => "type",
            Self::Status => "status",
            Self::BusinessRef => "business_ref",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::TenantId | Self::ExceptionId => FieldKind::Uuid,
            Self::ExceptionType | Self::Status | Self::BusinessRef => FieldKind::String,
        }
    }
}

pub use AccountInfoOrderFilterField as AccountInfoOrderField;
pub use AccountInfoQueryFilterField as AccountInfoFilterField;
pub use BalanceQueryFilterField as BalanceFilterField;
pub use CreditNoteQueryFilterField as CreditNoteFilterField;
pub use DebitNoteQueryFilterField as DebitNoteFilterField;
pub use DisputeQueryFilterField as DisputeFilterField;
pub use JournalEntryQueryFilterField as JournalEntryFilterField;
pub use JournalLineOrderFilterField as JournalLineOrderField;
pub use JournalLineQueryFilterField as JournalLineFilterField;
pub use RecognitionRunQueryFilterField as RecognitionRunFilterField;
pub use RefundOrderFilterField as RefundOrderField;
pub use RefundQueryFilterField as RefundFilterField;

#[cfg(test)]
#[path = "odata_tests.rs"]
mod odata_tests;
