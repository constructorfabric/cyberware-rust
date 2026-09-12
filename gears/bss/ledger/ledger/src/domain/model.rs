//! Repo-facing domain types: posting inputs, read-back records, and the
//! repository error. `i64` minor units throughout (decision C); SDK enums
//! carry the typed literals, stored as their canonical strings.

use bss_ledger_sdk::{AccountClass, MappingStatus, Side, SourceDocType};
use chrono::NaiveDate;
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// Identifying triple for a journal entry within a tenant period.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // all parts are *_id by nature of a key
pub struct EntryKey {
    pub tenant_id: Uuid,
    pub period_id: String,
    pub entry_id: Uuid,
}

/// Handle returned after a successful insert. `created_seq` is
/// DB-generated and read back from the inserted row.
#[domain_model]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryRef {
    pub entry_id: Uuid,
    pub created_seq: i64,
}

/// A journal entry header to insert (truth-table input). Mirrors the
/// `journal_entry` columns minus DB-generated ones (`created_seq`,
/// `row_hash`, `prev_hash`).
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewEntry {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: SourceDocType,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub reverses_period_id: Option<String>,
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    pub origin: String,
    pub posted_by_actor_id: Uuid,
    pub correlation_id: Uuid,
    pub rounding_evidence: JsonValue,
    /// The locked FX rate snapshot for this entry — **one rate per entry** (§4.3),
    /// stamped onto every line's `rate_snapshot_ref` by the journal repo. `None`
    /// for a single-currency entry (the `RateLocker` short-circuits and leaves
    /// functional NULL). Set by the S1/S2/S3 lock hook on a cross-currency post.
    pub rate_snapshot_ref: Option<Uuid>,
}

/// A journal line to insert (truth-table detail). Mirrors the
/// `journal_line` columns; `amount_minor`/`functional_amount_minor` are
/// integer minor units.
#[domain_model]
#[derive(Clone, Debug)]
pub struct NewLine {
    pub line_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub seller_tenant_id: Option<Uuid>,
    pub resource_tenant_id: Option<Uuid>,
    pub account_id: Uuid,
    pub account_class: AccountClass,
    pub gl_code: Option<String>,
    pub side: Side,
    pub amount_minor: i64,
    pub currency: String,
    /// Minor-unit scale (digits after the decimal). `u8`: a small non-negative
    /// count by construction; persisted as `smallint` at the entity boundary.
    pub currency_scale: u8,
    pub invoice_id: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub revenue_stream: Option<String>,
    pub mapping_status: MappingStatus,
    pub functional_amount_minor: Option<i64>,
    pub functional_currency: Option<String>,
    pub tax_jurisdiction: Option<String>,
    pub tax_filing_period: Option<String>,
    pub tax_rate_ref: Option<String>,
    pub legal_entity_id: Option<Uuid>,
    pub invoice_item_ref: Option<String>,
    pub sku_or_plan_ref: Option<String>,
    pub price_id: Option<String>,
    pub pricing_snapshot_ref: Option<String>,
    pub po_allocation_group: Option<String>,
    pub credit_grant_event_type: Option<String>,
    /// AR dispute sub-class (`ACTIVE`/`DISPUTED`), set on AR lines of a
    /// chargeback reclass; `None` on every other line.
    pub ar_status: Option<String>,
}

/// A read-back entry: header plus its lines. Strings carry the stored
/// literals; later phases parse them back into SDK enums at use sites.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRecord {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub reverses_period_id: Option<String>,
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    pub origin: String,
    pub posted_by_actor_id: Uuid,
    pub correlation_id: Uuid,
    pub rounding_evidence: JsonValue,
    pub created_seq: i64,
    pub lines: Vec<LineRecord>,
}

/// A read-back journal line.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRecord {
    pub line_id: Uuid,
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub payer_tenant_id: Uuid,
    pub seller_tenant_id: Option<Uuid>,
    pub resource_tenant_id: Option<Uuid>,
    pub account_id: Uuid,
    pub account_class: String,
    pub gl_code: Option<String>,
    pub side: String,
    pub amount_minor: i64,
    pub currency: String,
    pub currency_scale: i16,
    pub invoice_id: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub revenue_stream: Option<String>,
    pub mapping_status: String,
    pub functional_amount_minor: Option<i64>,
    pub functional_currency: Option<String>,
    pub tax_jurisdiction: Option<String>,
    pub tax_filing_period: Option<String>,
    pub tax_rate_ref: Option<String>,
    pub legal_entity_id: Option<Uuid>,
    pub invoice_item_ref: Option<String>,
    pub sku_or_plan_ref: Option<String>,
    pub price_id: Option<String>,
    pub pricing_snapshot_ref: Option<String>,
    pub po_allocation_group: Option<String>,
    pub credit_grant_event_type: Option<String>,
    /// AR dispute sub-class (`ACTIVE`/`DISPUTED`); `None` on non-dispute lines.
    pub ar_status: Option<String>,
}

/// A currency-scale registry row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrencyScaleRow {
    pub tenant_id: Uuid,
    pub currency: String,
    pub minor_units: i16,
    /// Per-currency plausible maximum in MAJOR units (resolved; the default
    /// 10^12 stands in when the request omits it). Governs the i64 headroom
    /// guard at registration.
    pub plausible_max_major: i64,
    pub source: String,
}

/// A chart-of-accounts row.
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub account_id: Uuid,
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub account_class: String,
    pub currency: String,
    pub revenue_stream: Option<String>,
    pub normal_side: String,
    pub may_go_negative: bool,
    pub lifecycle_state: String,
}

/// A fiscal-calendar config row (per legal entity).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiscalCalendarRow {
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub fiscal_tz: String,
    pub granularity: String,
    pub fy_start_month: i16,
    /// The legal entity's functional (books) currency, ISO-4217 (S5-F3). `None` →
    /// single-currency tenant (no FX translation).
    pub functional_currency: Option<String>,
}

/// A fiscal-period row (per legal entity + period).
#[domain_model]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiscalPeriodRow {
    pub tenant_id: Uuid,
    pub legal_entity_id: Uuid,
    pub period_id: String,
    pub fiscal_tz: String,
    pub status: String,
}

/// Errors surfaced by the foundation repositories.
#[domain_model]
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// Underlying database / scope failure.
    #[error("ledger repo db error: {0}")]
    Db(String),
    /// A just-inserted row could not be read back (invariant breach).
    #[error("ledger repo: row vanished after insert: {0}")]
    RowVanished(String),
    /// Currency scale would overflow i64 headroom at registration (A4/I-10).
    #[error("ledger repo: currency scale out of range: {0}")]
    ScaleOutOfRange(String),
    /// Scale change rejected: postings exist for the currency (maps to
    /// RFC-9457 `CURRENCY_SCALE_LOCKED`).
    #[error("ledger repo: currency scale locked: {0}")]
    CurrencyScaleLocked(String),
    /// A payment money-out counter cap CHECK was violated — an allocation /
    /// refund increment would push the per-payment running total past
    /// `settled_minor` (or a refund past its allocated amount). The sidecar
    /// maps this to `ALLOCATION_EXCEEDS_SETTLED`.
    #[error("ledger repo: payment money-out cap exceeded: {0}")]
    MoneyOutCapExceeded(String),
    /// A dispute-outcome advance matched no row that was still `OPENED` at the
    /// requested cycle — the dispute was concurrently resolved (a `won`/`lost`
    /// race) or the outcome targets a stale cycle. The sidecar maps this to
    /// `INVALID_DISPUTE_PHASE` (a non-retryable precondition failure), NOT a 500.
    #[error("ledger repo: dispute not OPENED at the requested cycle: {0}")]
    DisputeNotOpen(String),
}
