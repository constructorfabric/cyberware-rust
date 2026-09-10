//! Wire DTOs for the seller-provisioning endpoint. These own serde/utoipa
//! (via `#[toolkit_macros::api_dto]`) so the SDK value types
//! (`bss_ledger_sdk::provisioning`) stay infra-free. The `api_dto` macro is
//! the platform-mandated DTO wrapper (dylint DE0203) and fixes the wire shape
//! to `snake_case` — the uniform vhp-core REST convention, overriding the
//! architecture doc's illustrative camelCase.

use std::str::FromStr;

use chrono::NaiveDate;
use toolkit::api::canonical_prelude::{CanonicalError, resource_error};
use uuid::Uuid;

use bss_ledger_sdk::{
    AccountClass, AccountInfo, BalanceView, EntryView, FiscalCalendarSpec, Granularity, LineView,
    ProvisionAccount, ProvisionCurrencyScale, ProvisionOutcome, ProvisionRequest, Side,
};

use crate::domain::approval::policy::{DualControlPolicy, PolicyVersion};
use crate::domain::error::DomainError;
use crate::domain::instant::rfc3339;
use crate::domain::invoice::aging::AgingBucket;
use crate::domain::invoice::builder::{InvoiceItem, PostedInvoice, TaxBreakdown};
use crate::domain::recognition::input::{RecognitionInput, RecognitionTiming};
use time::OffsetDateTime;

/// One chart-of-accounts row to seed.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct AccountDto {
    pub account_class: String,
    pub currency: String,
    pub revenue_stream: Option<String>,
    pub normal_side: String,
    pub may_go_negative: Option<bool>,
}

/// One non-ISO currency-scale row to seed.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CurrencyScaleDto {
    pub currency: String,
    pub minor_units: i16,
    /// Per-currency plausible maximum in MAJOR units; omit for the default
    /// `10^12` (max scale 6). A higher-precision currency (e.g. BTC scale 8)
    /// passes a smaller cap (e.g. `21_000_000`) so its scale fits `i64` headroom.
    pub plausible_max_major: Option<i64>,
    pub source: Option<String>,
}

/// The fiscal-calendar config to seed.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct FiscalCalendarDto {
    pub timezone: String,
    pub granularity: String,
    pub fy_start: u8,
    /// The legal entity's functional (books) currency, ISO-4217 (S5-F3); omit for
    /// a single-currency tenant.
    pub functional_currency: Option<String>,
}

/// The provisioning request body: the target seller tenant, accounts, currency
/// scales, and the fiscal-calendar config to seed in one transaction. The
/// tenant is carried in the **body** (not the path) — the gear's REST surface is
/// tenant-from-context/body per the vhp-core convention (RBAC/RMS), and a
/// provision targets a seller the caller authorizes via the PEP gate.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ProvisioningRequestDto {
    pub tenant_id: Uuid,
    pub accounts: Vec<AccountDto>,
    pub currency_scales: Vec<CurrencyScaleDto>,
    pub fiscal_calendar: FiscalCalendarDto,
}

/// A chart-of-accounts entry in a response: coordinate + persistent `account_id`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AccountInfoDto {
    pub account_id: Uuid,
    pub account_class: String,
    pub currency: String,
    pub revenue_stream: Option<String>,
    pub lifecycle_state: String,
}

impl From<AccountInfo> for AccountInfoDto {
    fn from(a: AccountInfo) -> Self {
        Self {
            account_id: a.account_id,
            account_class: a.account_class.as_str().to_owned(),
            currency: a.currency,
            revenue_stream: a.revenue_stream,
            lifecycle_state: a.lifecycle_state,
        }
    }
}

// The `GET …/accounts` response is now the canonical `toolkit_odata::Page<AccountInfoDto>`
// envelope (`items` + `page_info`), built in the handler — the bespoke
// `AccountListDto { accounts }` wrapper is gone (RBAC list pattern).

/// The provisioning result: the accounts THIS call created + per-grain
/// created-vs-existing counts.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ProvisioningResultDto {
    pub accounts: Vec<AccountInfoDto>,
    pub accounts_created: u32,
    pub accounts_existing: u32,
    pub scales_created: u32,
    pub scales_existing: u32,
    pub calendar_created: bool,
    pub period_id: String,
    pub period_created: bool,
}

impl From<ProvisionOutcome> for ProvisioningResultDto {
    fn from(o: ProvisionOutcome) -> Self {
        Self {
            accounts: o.accounts.into_iter().map(AccountInfoDto::from).collect(),
            accounts_created: o.accounts_created,
            accounts_existing: o.accounts_existing,
            scales_created: o.scales_created,
            scales_existing: o.scales_existing,
            calendar_created: o.calendar_created,
            period_id: o.period_id,
            period_created: o.period_created,
        }
    }
}

/// Default currency-scale `source` when the caller omits it.
const DEFAULT_SCALE_SOURCE: &str = "TENANT";

impl ProvisioningRequestDto {
    /// Lower the wire DTO into the infra-free SDK [`ProvisionRequest`],
    /// parsing the string-typed enum fields (`account_class`, `normal_side`,
    /// `granularity`) into their SDK enums. The target tenant is taken from the
    /// body's own `tenant_id`.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when an `account_class`,
    /// `normal_side`, or `granularity` literal does not parse to a known SDK
    /// enum value.
    pub fn into_request(self) -> Result<ProvisionRequest, DomainError> {
        let accounts = self
            .accounts
            .into_iter()
            .map(|a| {
                let account_class = AccountClass::from_str(&a.account_class).map_err(|e| {
                    DomainError::InvalidRequest(format!("invalid account_class: {e}"))
                })?;
                let normal_side = Side::from_str(&a.normal_side).map_err(|e| {
                    DomainError::InvalidRequest(format!("invalid normal_side: {e}"))
                })?;
                Ok(ProvisionAccount {
                    account_class,
                    currency: a.currency,
                    revenue_stream: a.revenue_stream,
                    normal_side,
                    may_go_negative: a.may_go_negative.unwrap_or(false),
                })
            })
            .collect::<Result<Vec<_>, DomainError>>()?;

        let currency_scales = self
            .currency_scales
            .into_iter()
            .map(|s| {
                // Narrow the request scale to `u8` at the boundary: a negative or
                // > 255 value is not a valid minor-unit scale and is rejected as
                // InvalidArgument here (not deep in the upsert headroom guard).
                let minor_units = u8::try_from(s.minor_units).map_err(|_| {
                    DomainError::ScaleOutOfRange(format!(
                        "currency {} scale {} is out of range (0..=255)",
                        s.currency, s.minor_units
                    ))
                })?;
                Ok(ProvisionCurrencyScale {
                    currency: s.currency,
                    minor_units,
                    plausible_max_major: s.plausible_max_major,
                    source: s.source.unwrap_or_else(|| DEFAULT_SCALE_SOURCE.to_owned()),
                })
            })
            .collect::<Result<Vec<_>, DomainError>>()?;

        let granularity =
            Granularity::parse(&self.fiscal_calendar.granularity).ok_or_else(|| {
                DomainError::InvalidRequest(format!(
                    "invalid granularity: {:?}",
                    self.fiscal_calendar.granularity
                ))
            })?;

        Ok(ProvisionRequest {
            tenant_id: self.tenant_id,
            accounts,
            currency_scales,
            fiscal_calendar: FiscalCalendarSpec {
                timezone: self.fiscal_calendar.timezone,
                granularity,
                fy_start_month: self.fiscal_calendar.fy_start,
                functional_currency: self.fiscal_calendar.functional_currency,
            },
        })
    }
}

// ── Invoice-posting request DTOs (§6) ────────────────────────────────────────

/// One ex-tax billable line of an invoice to post. Money is the flat
/// `amount_minor` + `currency` pair the domain/storage carry (the ledger has no
/// composite money type). `catalog_class` / `contract_class` are the optional
/// GL-mapping inputs — a missing pair routes the line to `SUSPENSE`/`PENDING`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
// Field names mirror the domain `InvoiceItem` / `journal_line` columns verbatim;
// renaming to satisfy `struct_field_names` would diverge from the contract.
#[allow(clippy::struct_field_names)]
pub struct InvoiceItemDto {
    pub amount_minor_ex_tax: i64,
    pub currency: String,
    pub revenue_stream: String,
    /// Catalog-supplied GL class (the default mapping); a known `AccountClass`
    /// literal (e.g. `"REVENUE"`). `None` ⇒ no Catalog mapping.
    pub catalog_class: Option<String>,
    /// Contract-supplied GL class override; wins over `catalog_class`.
    pub contract_class: Option<String>,
    pub gl_code: Option<String>,
    /// Optional ASC 606 recognition spec (Slice 4). `None` ⇒ the item is fully
    /// recognized now (`deferred = 0`, the unchanged Variant-A contract). When
    /// present, the post derives the deferral schedule and splits the credit
    /// into Revenue (recognized now) + Contract-liability (deferred).
    pub recognition: Option<RecognitionInputDto>,
    pub invoice_item_ref: Option<String>,
    pub sku_or_plan_ref: Option<String>,
    pub price_id: Option<String>,
    pub pricing_snapshot_ref: Option<String>,
}

impl InvoiceItemDto {
    /// Lower one item DTO into the domain [`InvoiceItem`], parsing the optional
    /// `catalog_class` / `contract_class` literals into [`AccountClass`] and the
    /// optional `recognition` block into a [`RecognitionInput`].
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when a class literal does not parse, or
    /// when the `recognition` block carries an invalid timing (see
    /// [`RecognitionInputDto::into_domain`]).
    fn into_domain(self) -> Result<InvoiceItem, DomainError> {
        // Money invariant: an ex-tax line amount is non-negative. A negative
        // value is not just wrong accounting — it drives the recognition split's
        // `deferred.clamp(0, amount)` into `min > max` (a panic) downstream, so
        // reject it here at the wire boundary as a 400, never deeper.
        if self.amount_minor_ex_tax < 0 {
            return Err(DomainError::InvalidRequest(format!(
                "invoice item amount_minor_ex_tax must be non-negative, got {}",
                self.amount_minor_ex_tax
            )));
        }
        // The line currency is stamped verbatim on the journal line and never
        // re-parsed downstream; screen a malformed code here (a 400) rather than let
        // it silently match zero currency-scale rows / land in a persisted column.
        check_currency_code("currency", &self.currency)?;
        let catalog_class = parse_opt_account_class("catalog_class", self.catalog_class)?;
        let contract_class = parse_opt_account_class("contract_class", self.contract_class)?;
        let recognition = self
            .recognition
            .map(RecognitionInputDto::into_domain)
            .transpose()?;
        Ok(InvoiceItem {
            amount_minor_ex_tax: self.amount_minor_ex_tax,
            // The deferred portion is DERIVED server-side by the recognition
            // builder in the post orchestrator (never trusted from the wire);
            // seed `0` here.
            deferred_minor: 0,
            currency: self.currency,
            revenue_stream: self.revenue_stream,
            catalog_class,
            contract_class,
            gl_code: self.gl_code,
            recognition,
            invoice_item_ref: self.invoice_item_ref,
            sku_or_plan_ref: self.sku_or_plan_ref,
            price_id: self.price_id,
            pricing_snapshot_ref: self.pricing_snapshot_ref,
        })
    }
}

/// The optional per-item ASC 606 recognition spec on the wire (Slice 4). Maps to
/// the domain [`RecognitionInput`]; absence on an item ⇒ no deferral. `timing` is
/// `"point_in_time"` (no deferral, explicit) or `"straight_line"` (defer the
/// whole ex-tax amount over `periods` equal segments from `first_period_id`,
/// defaulted to the invoice period when omitted). The optional refs/flags carry
/// PO / SSP / VC / subscription context onto the materialized schedule.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
// The `*_ref` / `*_group` fields mirror the `recognition_schedule` column names
// verbatim (the storage contract); renaming to satisfy `struct_field_names`
// would diverge from `RecognitionInput` / `NewSchedule`.
#[allow(clippy::struct_field_names)]
pub struct RecognitionInputDto {
    /// The deferral+timing policy version stamped immutably on the schedule.
    pub policy_ref: String,
    /// `"point_in_time"` | `"straight_line"` — the recognition timing pattern.
    pub timing: String,
    /// Straight-line only: number of equal recognition segments (`>= 1`).
    /// Required for `timing = "straight_line"`, ignored for `"point_in_time"`.
    pub periods: Option<u32>,
    /// Straight-line only: first fiscal period (`YYYYMM`) the schedule recognizes
    /// into; `None` ⇒ defaulted to the invoice period.
    pub first_period_id: Option<String>,
    /// The PO / allocation group this line books under (audit, §4.7).
    pub po_allocation_group: Option<String>,
    /// `true` for a genuine multi-performance-obligation line — the only case
    /// where a missing/unresolvable SSP snapshot blocks the post (§4.4). `None` ⇒
    /// `false` (single-PO).
    pub multi_po: Option<bool>,
    /// The SSP snapshot ref pinned at contract inception; required + resolvable
    /// for a `multi_po` line (else `SSP_SNAPSHOT_REQUIRED`).
    pub ssp_snapshot_ref: Option<String>,
    /// The subscription/entitlement this obligation belongs to (audit).
    pub subscription_ref: Option<String>,
    /// Variable-consideration estimate ref (carried only — VC posting is OUT of
    /// the MVP, N-revrec-4).
    pub vc_estimate_ref: Option<String>,
    /// Variable-consideration method ref (carried only — VC posting is OUT of
    /// the MVP, N-revrec-4).
    pub vc_method_ref: Option<String>,
    /// `true` iff the Catalog SKU is flagged immaterial-one-shot-eligible (an R4
    /// exemption precondition). `None` ⇒ `false` (not eligible).
    pub immaterial_one_shot_sku: Option<bool>,
}

/// The two recognition-timing wire literals.
const TIMING_POINT_IN_TIME: &str = "point_in_time";
const TIMING_STRAIGHT_LINE: &str = "straight_line";

impl RecognitionInputDto {
    /// Lower the wire recognition block into the domain [`RecognitionInput`],
    /// parsing the `timing` literal into [`RecognitionTiming`]. A
    /// `"straight_line"` requires `periods` (`>= 1`); a `"point_in_time"` ignores
    /// `periods` / `first_period_id`.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `timing` is neither
    /// `"point_in_time"` nor `"straight_line"`, or when `"straight_line"` omits a
    /// `periods >= 1`.
    fn into_domain(self) -> Result<RecognitionInput, DomainError> {
        let timing = match self.timing.as_str() {
            TIMING_POINT_IN_TIME => RecognitionTiming::PointInTime,
            TIMING_STRAIGHT_LINE => {
                let periods = self.periods.filter(|p| *p >= 1).ok_or_else(|| {
                    DomainError::InvalidRequest(
                        "recognition straight_line requires `periods` >= 1".to_owned(),
                    )
                })?;
                RecognitionTiming::StraightLine {
                    periods,
                    first_period_id: self.first_period_id,
                }
            }
            other => {
                return Err(DomainError::InvalidRequest(format!(
                    "unknown recognition timing {other:?} (expected \
                     \"point_in_time\" or \"straight_line\")"
                )));
            }
        };
        Ok(RecognitionInput {
            policy_ref: self.policy_ref,
            timing,
            po_allocation_group: self.po_allocation_group,
            multi_po: self.multi_po.unwrap_or(false),
            ssp_snapshot_ref: self.ssp_snapshot_ref,
            subscription_ref: self.subscription_ref,
            vc_estimate_ref: self.vc_estimate_ref,
            vc_method_ref: self.vc_method_ref,
            immaterial_one_shot_sku: self.immaterial_one_shot_sku.unwrap_or(false),
        })
    }
}

/// One tax component of an invoice to post, already computed by the tax engine.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct TaxBreakdownDto {
    pub amount_minor: i64,
    pub currency: String,
    pub tax_jurisdiction: String,
    pub tax_filing_period: String,
    pub tax_rate_ref: Option<String>,
}

impl From<TaxBreakdownDto> for TaxBreakdown {
    fn from(t: TaxBreakdownDto) -> Self {
        Self {
            amount_minor: t.amount_minor,
            currency: t.currency,
            tax_jurisdiction: t.tax_jurisdiction,
            tax_filing_period: t.tax_filing_period,
            tax_rate_ref: t.tax_rate_ref,
        }
    }
}

/// The `POST /journal-entries` request body: a fully-recognized invoice
/// (Variant A) to post. The target seller ledger is the body's own `tenant_id`
/// (tenant in body, not path — the vhp-core REST convention); the `(entry,
/// post)` PEP gate authorizes it.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct PostInvoiceRequestDto {
    /// The seller tenant whose ledger this posts into (= `entry.tenant_id`);
    /// the PEP gate target. Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub invoice_id: String,
    pub payer_tenant_id: Uuid,
    pub resource_tenant_id: Option<Uuid>,
    pub effective_at: NaiveDate,
    /// AR due date stamped on the AR line (drives AR-aging); `None` ⇒ due on
    /// posting.
    pub due_date: Option<NaiveDate>,
    /// The fiscal `period_id` (`YYYYMM`) the entry posts into.
    pub period_id: String,
    pub items: Vec<InvoiceItemDto>,
    pub tax: Vec<TaxBreakdownDto>,
    /// Caller-supplied trace token propagated onto the entry (NOT an authority
    /// claim — the poster identity is the authenticated subject, stamped
    /// server-side, never read from the body).
    pub correlation_id: Uuid,
}

impl PostInvoiceRequestDto {
    /// Lower the wire DTO into the domain [`PostedInvoice`], parsing each item's
    /// optional GL-class literals at the boundary (a bad literal is rejected as
    /// `InvalidArgument` ⇒ HTTP 400, not deep in the post path). The seller
    /// ledger is the body's own `tenant_id`; `posted_by_actor_id` is the
    /// authenticated subject (passed in), never trusted from the body.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when an item's `catalog_class` /
    /// `contract_class` literal does not parse to a known [`AccountClass`], or
    /// when the items/tax carry more than one currency (mixed-currency invoice).
    pub fn into_domain(self, posted_by_actor_id: Uuid) -> Result<PostedInvoice, DomainError> {
        let items = self
            .items
            .into_iter()
            .map(InvoiceItemDto::into_domain)
            .collect::<Result<Vec<_>, DomainError>>()?;
        // Tax amounts are non-negative for the same reason (the AR gross folds
        // `Σ items + Σ tax`; a negative tax line understates the receivable).
        if let Some(bad) = self.tax.iter().find(|t| t.amount_minor < 0) {
            return Err(DomainError::InvalidRequest(format!(
                "tax amount_minor must be non-negative, got {}",
                bad.amount_minor
            )));
        }
        // Each tax component's currency is stamped on its own line but lowers via a
        // plain `From` (no per-field guard), so this is the one place its code is
        // screened before it reaches a persisted line.
        for t in &self.tax {
            check_currency_code("tax.currency", &t.currency)?;
        }
        let tax: Vec<TaxBreakdown> = self.tax.into_iter().map(TaxBreakdown::from).collect();
        // A zero-line invoice (no items AND no tax) has nothing to post — the builder
        // would emit an empty entry. Reject it at the boundary as a 400 (an
        // items-empty-but-tax-present invoice is a legitimate tax-only posting).
        if items.is_empty() && tax.is_empty() {
            return Err(DomainError::InvalidRequest(
                "invoice must carry at least one item or tax line".to_owned(),
            ));
        }
        // Single-currency invariant: the builder stamps the reference currency on
        // every line, so a differing item/tax currency would be silently
        // misattributed. Reject it at the boundary (the reference = first item's
        // currency, else first tax's).
        if let Some(reference) = items
            .first()
            .map(|i| i.currency.as_str())
            .or_else(|| tax.first().map(|t| t.currency.as_str()))
            && (items.iter().any(|i| i.currency != reference)
                || tax.iter().any(|t| t.currency != reference))
        {
            return Err(DomainError::InvalidRequest(
                "mixed-currency invoice: all items and tax must share one currency".to_owned(),
            ));
        }
        Ok(PostedInvoice {
            invoice_id: self.invoice_id,
            payer_tenant_id: self.payer_tenant_id,
            resource_tenant_id: self.resource_tenant_id,
            seller_tenant_id: self.tenant_id,
            effective_at: self.effective_at,
            due_date: self.due_date,
            period_id: self.period_id,
            items,
            tax,
            posted_by_actor_id,
            correlation_id: self.correlation_id,
        })
    }
}

/// The `POST /journal-entries/{entryId}/reversals` request body. The reversed
/// entry is the `{entryId}` path id; the tenant is the caller's auth context.
/// The reversal lands in the supplied `period_id` (a still-OPEN period — it may
/// differ from the original's) effective `effective_at`; both default
/// server-side to the original's period / today when omitted.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ReversalRequestDto {
    /// Audit reason for the reversal (recorded by the caller's actor).
    pub reason: String,
    /// Target OPEN period for the reversal; `None` ⇒ the original's period.
    pub period_id: Option<String>,
    /// GL effective date of the reversal; `None` ⇒ today (UTC).
    pub effective_at: Option<NaiveDate>,
}

impl ReversalRequestDto {
    /// Cap the persisted audit `reason` free text at the boundary (this DTO has
    /// no lowering method — the handler calls this before the write).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `reason` exceeds [`MAX_FREE_TEXT_LEN`].
    pub(crate) fn validate(&self) -> Result<(), DomainError> {
        check_free_text("reason", &self.reason, MAX_FREE_TEXT_LEN)
    }
}

/// The `POST /journal-entries/{entryId}/mapping-corrections` request body: a
/// strict reversal of the mis-mapped original immediately followed by a
/// corrected re-post (`corrected_items` re-mapped to the right accounts). The
/// original is the `{entryId}` path id; the tenant is the caller's auth context.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct MappingCorrectionRequestDto {
    /// Audit reason for the correction.
    pub reason: String,
    /// Target OPEN period for both halves; `None` ⇒ the original's period.
    pub period_id: Option<String>,
    /// GL effective date; `None` ⇒ today (UTC).
    pub effective_at: Option<NaiveDate>,
    /// The corrected ex-tax lines, re-mapped to the right accounts.
    pub corrected_items: Vec<InvoiceItemDto>,
}

impl MappingCorrectionRequestDto {
    /// Lower the corrected items into domain [`InvoiceItem`]s (parsing the GL
    /// class literals at the boundary).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when an item's class literal does not
    /// parse.
    pub fn corrected_items_into_domain(&self) -> Result<Vec<InvoiceItem>, DomainError> {
        // The audit `reason` is persisted on the secured-audit record; cap the
        // unbounded free text at the boundary (this is the DTO's only lowering
        // method, so it is where the field is screened).
        check_free_text("reason", &self.reason, MAX_FREE_TEXT_LEN)?;
        self.corrected_items
            .iter()
            .cloned()
            .map(InvoiceItemDto::into_domain)
            .collect()
    }
}

/// The `PATCH /journal-entries/{entryId}/annotation` request body (Group 2B,
/// Variant C): set the typed controlled non-financial `description` note on the
/// entry (or one of its lines). `description` is screened for raw customer PII
/// before any write (`PII_IN_METADATA_VALUE`); `null` clears the note.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EntryAnnotationRequestDto {
    /// The controlled note. `null` clears it. Screened for raw customer PII.
    pub description: Option<String>,
    /// `ENTRY` (the default) or `LINE`. `LINE` requires `target_line_id`.
    pub target_kind: Option<String>,
    /// The target line id when `target_kind = "LINE"`; ignored for `ENTRY`.
    pub target_line_id: Option<Uuid>,
    /// Audit reason for the change (recorded on the secured-audit record).
    pub reason: String,
}

impl EntryAnnotationRequestDto {
    /// Cap the persisted `description` note and audit `reason` free text at the
    /// boundary (this DTO has no lowering method — the handler calls this before
    /// the write; PII screening of `description` runs separately downstream).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when either exceeds [`MAX_FREE_TEXT_LEN`].
    pub(crate) fn validate(&self) -> Result<(), DomainError> {
        if let Some(description) = &self.description {
            check_free_text("description", description, MAX_FREE_TEXT_LEN)?;
        }
        check_free_text("reason", &self.reason, MAX_FREE_TEXT_LEN)
    }
}

// ── Audit-retrieval response DTOs (Group 2C, AC #8) ──────────────────────────

/// The who/when/source/correlation dims of one posted entry, the
/// `GET …/audit/journal-entries/{entryId}` response (and one row of a document
/// history). A pure audit projection of `journal_entry`; carries no lines.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AuditEntryDto {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub posted_by_actor_id: Uuid,
    pub origin: String,
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub correlation_id: Uuid,
    pub reverses_entry_id: Option<Uuid>,
}

impl From<crate::infra::audit::retrieval::AuditEntryRecord> for AuditEntryDto {
    fn from(r: crate::infra::audit::retrieval::AuditEntryRecord) -> Self {
        Self {
            entry_id: r.entry_id,
            tenant_id: r.tenant_id,
            period_id: r.period_id,
            posted_by_actor_id: r.posted_by_actor_id,
            origin: r.origin,
            posted_at_utc: r.posted_at_utc,
            source_doc_type: r.source_doc_type,
            source_business_id: r.source_business_id,
            correlation_id: r.correlation_id,
            reverses_entry_id: r.reverses_entry_id,
        }
    }
}

/// The full posting history of one source document, the
/// `GET …/audit/documents/{sourceDocType}/{sourceBusinessId}/history` response:
/// every entry for that document plus any reversal / mapping-correction that
/// links to one of them, ordered by `created_seq`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DocumentHistoryDto {
    pub entries: Vec<AuditEntryDto>,
}

/// One scope-freeze row in a tamper-status read.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FreezeDto {
    pub scope: String,
    pub period_id: String,
    pub reason: String,
    #[serde(with = "rfc3339")]
    pub frozen_at: OffsetDateTime,
    pub set_by: String,
    pub cleared_by: Option<String>,
    #[serde(default, with = "rfc3339::option")]
    pub cleared_at: Option<OffsetDateTime>,
}

impl From<crate::infra::audit::retrieval::FreezeRecord> for FreezeDto {
    fn from(r: crate::infra::audit::retrieval::FreezeRecord) -> Self {
        Self {
            scope: r.scope,
            period_id: r.period_id,
            reason: r.reason,
            frozen_at: r.frozen_at,
            set_by: r.set_by,
            cleared_by: r.cleared_by,
            cleared_at: r.cleared_at,
        }
    }
}

/// The tamper-status of a resolved scope, the `GET …/audit/tamper-status`
/// response: `scope_frozen` (any ACTIVE freeze), the freeze rows, and a derived
/// `verified` (= `!scope_frozen`, the MVP derivation — see
/// [`crate::infra::audit::retrieval::AuditRetrievalReader::tamper_status_in_txn`]).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TamperStatusDto {
    pub scope_frozen: bool,
    pub freezes: Vec<FreezeDto>,
    pub verified: bool,
}

impl From<crate::infra::audit::retrieval::TamperStatusRecord> for TamperStatusDto {
    fn from(r: crate::infra::audit::retrieval::TamperStatusRecord) -> Self {
        Self {
            scope_frozen: r.scope_frozen,
            freezes: r.freezes.into_iter().map(FreezeDto::from).collect(),
            verified: r.verified,
        }
    }
}

// ── Audit-pack inquiry / export DTOs (Slice 6 Phase 4 Group 4A) ──────────────

/// The inquiry filter axes of an audit-pack request (all optional; an absent
/// field is "any"). `payer_tenant_id` / `account_class` filter the lines;
/// `period_id` / `legal_entity_id` filter the entry header. `period_id` is the
/// fiscal `YYYYMM` string the schema carries (NOT a `Uuid`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct InquiryFilterDto {
    pub payer_tenant_id: Option<Uuid>,
    pub period_id: Option<String>,
    pub account_class: Option<String>,
    pub legal_entity_id: Option<Uuid>,
}

impl From<InquiryFilterDto> for crate::infra::inquiry::InquiryFilter {
    fn from(f: InquiryFilterDto) -> Self {
        Self {
            payer_tenant_id: f.payer_tenant_id,
            period_id: f.period_id,
            account_class: f.account_class,
            legal_entity_id: f.legal_entity_id,
        }
    }
}

/// `POST …/audit/packs` request body: the inquiry filter, an optional
/// cross-tenant `target_scope` (the tenant to open — a different tenant triggers
/// the forensic elevation gate), and the machine `reason_code` (the free-text
/// reason is the `X-Investigation-Reason` header). A cross-tenant pack needs
/// both a reason header and a `reason_code`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct AuditPackRequestDto {
    pub filter: InquiryFilterDto,
    /// The tenant to open (defaults to the caller's own; a different tenant
    /// triggers the forensic cross-tenant elevation gate).
    pub target_scope: Option<Uuid>,
    /// Machine-readable investigation reason code (required for a cross-tenant
    /// pack).
    pub reason_code: Option<String>,
}

impl AuditPackRequestDto {
    /// Cap the machine `reason_code` at the boundary (this DTO has no lowering
    /// method — the handler calls this before the export).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `reason_code` exceeds
    /// [`MAX_REASON_CODE_LEN`].
    pub(crate) fn validate(&self) -> Result<(), DomainError> {
        if let Some(reason_code) = &self.reason_code {
            check_free_text("reason_code", reason_code, MAX_REASON_CODE_LEN)?;
        }
        Ok(())
    }
}

/// Audit-pack export resource (Slice 6 §5/§10). `POST …/audit/packs` returns
/// this at `202 Accepted` (without `csv` — a job summary) with a `Location` to
/// `GET …/audit/packs/{exportId}`, which returns it again with `csv` populated
/// once `status = succeeded`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AuditPackExportDto {
    pub export_id: Uuid,
    /// `accepted` | `processing` | `succeeded` | `failed`.
    pub status: String,
    /// The tenant whose ledger was exported (= the caller's own on a routine
    /// export).
    pub target_tenant_id: Uuid,
    /// Data-row count of the CSV (excludes the header row).
    pub row_count: u64,
    /// The materialized CSV document; present once `status = succeeded` (omitted
    /// from the `202` create response — poll the `Location`).
    pub csv: Option<String>,
    /// Failure diagnostic when `status = failed`.
    pub error_detail: Option<String>,
    #[serde(with = "rfc3339")]
    pub created_at_utc: OffsetDateTime,
    #[serde(default, with = "rfc3339::option")]
    pub completed_at_utc: Option<OffsetDateTime>,
}

impl AuditPackExportDto {
    /// The `202`-create summary: the job identity + state, WITHOUT the CSV body
    /// (the client fetches the full resource — with `csv` — from the `Location`).
    #[must_use]
    pub fn summary(model: &crate::infra::storage::entity::audit_pack_export::Model) -> Self {
        Self {
            export_id: model.export_id,
            status: model.status.clone(),
            target_tenant_id: model.target_tenant_id,
            row_count: u64::try_from(model.row_count).unwrap_or(0),
            csv: None,
            error_detail: model.error_detail.clone(),
            created_at_utc: model.created_at_utc,
            completed_at_utc: model.completed_at_utc,
        }
    }
}

impl From<crate::infra::storage::entity::audit_pack_export::Model> for AuditPackExportDto {
    /// The full polled resource: includes the materialized `csv` (decoded as
    /// UTF-8 — the exporter writes UTF-8) when present.
    fn from(model: crate::infra::storage::entity::audit_pack_export::Model) -> Self {
        Self {
            export_id: model.export_id,
            status: model.status,
            target_tenant_id: model.target_tenant_id,
            row_count: u64::try_from(model.row_count).unwrap_or(0),
            csv: model
                .csv
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            error_detail: model.error_detail,
            created_at_utc: model.created_at_utc,
            completed_at_utc: model.completed_at_utc,
        }
    }
}

/// Parse an optional `AccountClass` literal carried on a request, mapping a bad
/// literal to [`DomainError::InvalidRequest`] (⇒ HTTP 400). `field` names the
/// offending field for the diagnostic.
fn parse_opt_account_class(
    field: &str,
    value: Option<String>,
) -> Result<Option<AccountClass>, DomainError> {
    match value {
        None => Ok(None),
        Some(literal) => AccountClass::from_str(&literal)
            .map(Some)
            .map_err(|e| DomainError::InvalidRequest(format!("invalid {field}: {e}"))),
    }
}

// ── Invoice-posting response DTOs (§6) ───────────────────────────────────────

/// Reference to a posted (or idempotently replayed) entry. `replayed` is `true`
/// when the call matched a prior post (the handler then renders `200`, not
/// `201`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PostingRefDto {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for PostingRefDto {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// One recognition schedule materialized by an invoice-post, echoed in the
/// response so a REST client learns the server-minted `schedule_id` (a `UUIDv7`
/// string) without subscribing to the event bus. One per deferred item-stream;
/// a point-in-time item produces none.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MaterializedScheduleDto {
    /// The server-minted schedule id (`(tenant, schedule_id)` PK tail).
    pub schedule_id: String,
    /// The revenue stream the schedule books to.
    pub revenue_stream: String,
    /// The Contract-liability invoice line the schedule draws down.
    pub source_invoice_item_ref: String,
}

impl From<bss_ledger_sdk::RecognitionScheduleSummaryView> for MaterializedScheduleDto {
    fn from(v: bss_ledger_sdk::RecognitionScheduleSummaryView) -> Self {
        Self {
            schedule_id: v.schedule_id,
            revenue_stream: v.revenue_stream,
            source_invoice_item_ref: v.source_invoice_item_ref,
        }
    }
}

/// The `POST /journal-entries` (invoice-post) response: the posted-entry
/// reference (`entry_id`, `created_seq`, `replayed`) plus the recognition
/// schedules materialized in the SAME transaction (`schedules`, empty when the
/// invoice carried no deferred items). Extends the bare posting reference with
/// the discovery half of the recognition-schedule surface — the minted
/// `schedule_id`s are otherwise only observable on the event bus. On an
/// idempotent replay (`replayed = true`) the schedules are those the original
/// post materialized.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PostInvoiceResponseDto {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
    pub schedules: Vec<MaterializedScheduleDto>,
}

/// A read-back journal line in a response (a row of [`EntryDto`] or of the
/// `GET /journal-lines` `Page<LineDto>`). Enum fields render as their canonical
/// string literals (`account_class`, `side`, `mapping_status`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct LineDto {
    pub line_id: Uuid,
    pub entry_id: Uuid,
    pub payer_tenant_id: Uuid,
    pub account_id: Uuid,
    pub account_class: String,
    pub gl_code: Option<String>,
    pub side: String,
    pub amount_minor: i64,
    pub currency: String,
    pub currency_scale: u8,
    pub invoice_id: Option<String>,
    pub due_date: Option<NaiveDate>,
    pub revenue_stream: Option<String>,
    pub mapping_status: String,
    pub tax_jurisdiction: Option<String>,
    pub tax_filing_period: Option<String>,
    /// AR dispute sub-class (`ACTIVE`/`DISPUTED`); absent on non-dispute lines.
    pub ar_status: Option<String>,
}

impl From<LineView> for LineDto {
    fn from(l: LineView) -> Self {
        Self {
            line_id: l.line_id,
            entry_id: l.entry_id,
            payer_tenant_id: l.payer_tenant_id,
            account_id: l.account_id,
            account_class: l.account_class.as_str().to_owned(),
            gl_code: l.gl_code,
            side: l.side.as_str().to_owned(),
            amount_minor: l.amount_minor,
            currency: l.currency,
            currency_scale: l.currency_scale,
            invoice_id: l.invoice_id,
            due_date: l.due_date,
            revenue_stream: l.revenue_stream,
            mapping_status: l.mapping_status.as_str().to_owned(),
            tax_jurisdiction: l.tax_jurisdiction,
            tax_filing_period: l.tax_filing_period,
            ar_status: l.ar_status,
        }
    }
}

/// A read-back journal entry (header + its lines), the `GET
/// /journal-entries/{entryId}` response. Carries the audit dims
/// (`posted_at_utc`, `posted_by_actor_id`, `origin`, `correlation_id`) +
/// `reverses_entry_id` a caller needs to audit / build a reversal.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct EntryDto {
    pub entry_id: Uuid,
    pub tenant_id: Uuid,
    pub period_id: String,
    pub entry_currency: String,
    pub source_doc_type: String,
    pub source_business_id: String,
    pub reverses_entry_id: Option<Uuid>,
    pub reverses_period_id: Option<String>,
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    pub posted_by_actor_id: Uuid,
    pub origin: String,
    pub correlation_id: Uuid,
    pub created_seq: i64,
    pub lines: Vec<LineDto>,
}

impl From<EntryView> for EntryDto {
    fn from(e: EntryView) -> Self {
        Self {
            entry_id: e.entry_id,
            tenant_id: e.tenant_id,
            period_id: e.period_id,
            entry_currency: e.entry_currency,
            source_doc_type: e.source_doc_type.as_str().to_owned(),
            source_business_id: e.source_business_id,
            reverses_entry_id: e.reverses_entry_id,
            reverses_period_id: e.reverses_period_id,
            posted_at_utc: e.posted_at_utc,
            effective_at: e.effective_at,
            posted_by_actor_id: e.posted_by_actor_id,
            origin: e.origin,
            correlation_id: e.correlation_id,
            created_seq: e.created_seq,
            lines: e.lines.into_iter().map(LineDto::from).collect(),
        }
    }
}

// The `GET /journal-lines` response is now the canonical
// `toolkit_odata::Page<LineDto>` envelope (`items` + `page_info` cursor
// metadata), built in the handler — the bespoke
// `JournalLinePageDto { items, next_cursor }` wrapper is gone (RBAC list
// pattern: the page envelope is the shared toolkit type, not a per-gear DTO).

/// A read-back account-balance row in the `GET /balances` response. Carries both
/// the transaction-currency `balance_minor` and the Slice-5 functional valuation:
/// `functional_balance_minor` is `null` on a single-currency grain (the
/// `?valuation=functional` read falls back to `balance_minor` by identity, P1
/// decision 8).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct BalanceDto {
    pub account_id: Uuid,
    pub account_class: String,
    pub currency: String,
    pub balance_minor: i64,
    /// Functional-currency carried balance; `null` on a single-currency grain
    /// (functional ≡ transaction).
    pub functional_balance_minor: Option<i64>,
    /// Functional currency of `functional_balance_minor`; `null` on a
    /// single-currency grain.
    pub functional_currency: Option<String>,
}

impl From<BalanceView> for BalanceDto {
    fn from(b: BalanceView) -> Self {
        Self {
            account_id: b.account_id,
            account_class: b.account_class.as_str().to_owned(),
            currency: b.currency,
            balance_minor: b.balance_minor,
            functional_balance_minor: b.functional_balance_minor,
            functional_currency: b.functional_currency,
        }
    }
}

// The `GET /balances` response is now the canonical
// `toolkit_odata::Page<BalanceDto>` envelope (`items` + `page_info`), built in
// the handler — the bespoke `BalanceListDto { balances }` wrapper is gone.

/// One aged AR grain in the `GET /balances/ar-aging` response: the outstanding
/// receivable for a `(payer, currency, bucket)`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AgingBucketDto {
    pub payer_tenant_id: Uuid,
    pub currency: String,
    pub bucket: String,
    pub amount_minor: i64,
}

impl From<AgingBucket> for AgingBucketDto {
    fn from(b: AgingBucket) -> Self {
        Self {
            payer_tenant_id: b.payer_tenant_id,
            currency: b.currency,
            bucket: b.bucket,
            amount_minor: b.amount_minor,
        }
    }
}

/// The `GET /balances/ar-aging` response: the open AR aged into buckets.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ArAgingDto {
    pub buckets: Vec<AgingBucketDto>,
}

impl From<Vec<AgingBucket>> for ArAgingDto {
    fn from(buckets: Vec<AgingBucket>) -> Self {
        Self {
            buckets: buckets.into_iter().map(AgingBucketDto::from).collect(),
        }
    }
}

// ── Payment request/response DTOs (§6, money-in / money-out) ─────────────────

/// The `POST /payments` request body: a settled payment to record (the
/// **money-in** side). The target seller ledger is the body's own `tenant_id`
/// (tenant in body, not path — the vhp-core REST convention); the `(payment,
/// write)` PEP gate authorizes it. `scale` is the payment's currency scale as
/// known to the caller — advisory; the ledger resolves the authoritative
/// per-line scale from the provisioned currency config. `effective_at` `None`
/// ⇒ the receipt is stamped at post time.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SettlePaymentRequest {
    /// The seller tenant whose ledger this settles into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// External payment identity — the idempotency key (a re-settle replays).
    pub payment_id: String,
    /// Gross received in minor units (what the payer was charged).
    pub gross_minor: i64,
    /// Processor's withheld cut in minor units (`<= gross`).
    pub fee_minor: i64,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// Receipt instant; `None` ⇒ stamped at post time (current-month period).
    #[serde(default, with = "rfc3339::option")]
    pub effective_at: Option<OffsetDateTime>,
}

/// Max length of a client-supplied business id — matches the `varchar(128)`
/// columns the ledger persists these ids in. An over-long (or empty) id is
/// rejected at the boundary as a clean 400 rather than surfacing as a 500 from
/// the column write.
pub const MAX_BUSINESS_ID_LEN: usize = 128;

/// Validate a client-supplied business id: non-empty and within the
/// `varchar(128)` column bound. `field` names the offending field in the 400.
///
/// # Errors
/// [`DomainError::InvalidRequest`] when `value` is empty or longer than
/// [`MAX_BUSINESS_ID_LEN`] bytes.
fn validate_business_id(field: &str, value: &str) -> Result<(), DomainError> {
    if value.is_empty() {
        return Err(DomainError::InvalidRequest(format!(
            "{field} must not be empty"
        )));
    }
    if value.len() > MAX_BUSINESS_ID_LEN {
        return Err(DomainError::InvalidRequest(format!(
            "{field} must be at most {MAX_BUSINESS_ID_LEN} bytes, got {}",
            value.len()
        )));
    }
    Ok(())
}

impl SettlePaymentRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::SettlePayment`],
    /// validating the client-supplied `payment_id` length at the boundary.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `payment_id` is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`].
    pub fn into_sdk(self) -> Result<bss_ledger_sdk::SettlePayment, DomainError> {
        validate_business_id("payment_id", &self.payment_id)?;
        check_currency_code("currency", &self.currency)?;
        Ok(bss_ledger_sdk::SettlePayment {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            payment_id: self.payment_id,
            gross_minor: self.gross_minor,
            fee_minor: self.fee_minor,
            currency: self.currency,
            scale: self.scale,
            effective_at: self.effective_at,
        })
    }
}

/// The `POST /payments` response: a reference to the settlement posting. A
/// fresh settle renders `201`, an idempotent replay `200` (the handler reads
/// `replayed`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct SettlePaymentResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for SettlePaymentResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// The `POST /payments/{payment_id}/returns` request body: claw a settled
/// receipt back out (the reversal of a money-in). `{payment_id}` (the original
/// settled payment) comes from the PATH; `psp_return_id` is the idempotency key
/// (a re-post replays). `scale` is advisory; `effective_at` `None` ⇒ stamped at
/// post time.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ReturnPaymentRequest {
    /// The seller tenant whose ledger this returns within; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// External return identity — the idempotency key.
    pub psp_return_id: String,
    /// Amount returned in minor units (`> 0`).
    pub amount_minor: i64,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// Return instant; `None` ⇒ stamped at post time (current-month period).
    #[serde(default, with = "rfc3339::option")]
    pub effective_at: Option<OffsetDateTime>,
}

impl ReturnPaymentRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::ReturnPayment`], binding
    /// `payment_id` from the request PATH (the body carries no payment id —
    /// mirrors [`AllocatePaymentRequest::into_sdk`]) and validating the
    /// client-supplied id lengths at the boundary.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `payment_id` (path) or `psp_return_id`
    /// (body) is empty or exceeds [`MAX_BUSINESS_ID_LEN`].
    pub fn into_sdk(
        self,
        payment_id: String,
    ) -> Result<bss_ledger_sdk::ReturnPayment, DomainError> {
        validate_business_id("payment_id", &payment_id)?;
        validate_business_id("psp_return_id", &self.psp_return_id)?;
        check_currency_code("currency", &self.currency)?;
        Ok(bss_ledger_sdk::ReturnPayment {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            payment_id,
            psp_return_id: self.psp_return_id,
            amount_minor: self.amount_minor,
            currency: self.currency,
            scale: self.scale,
            effective_at: self.effective_at,
        })
    }
}

/// The `POST /payments/{payment_id}/returns` response: a reference to the return
/// posting. A fresh return renders `201`, an idempotent replay `200` (the
/// handler reads `replayed`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReturnPaymentResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for ReturnPaymentResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

// ── Chargeback (dispute) request/response DTOs (§4.5, dispute state machine) ──

/// The `POST /disputes/{dispute_id}/phases` request body: record one phase of a
/// chargeback dispute. `{dispute_id}` (the dispute's external id) comes from the
/// PATH; the target seller ledger is the body's own `tenant_id` (tenant in body,
/// not path — the vhp-core REST convention); the `(dispute, write)` PEP gate
/// authorizes it. The LEDGER chooses the variant at `opened` from `funds_at_open`
/// (`"withheld"` ⇒ cash-hold, `"not_moved"` ⇒ AR-reclass); `phase` is one of
/// `"opened" | "won" | "lost" | "partial"` (Group B implements `opened`).
/// `cycle` defaults to 1 and increments on a re-open. `invoice_id` is required
/// for an AR-reclass `opened` (the disputed `(payer, invoice)` grain), ignored
/// for cash-hold. `scale` is advisory; `effective_at` `None` ⇒ stamped at post
/// time.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RecordDisputePhaseRequest {
    /// The seller tenant whose ledger this disputes within; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// The disputed payment.
    pub payment_id: String,
    /// The disputed `(payer, invoice)` AR grain — required for an AR-reclass
    /// `opened`, ignored for cash-hold.
    pub invoice_id: Option<String>,
    /// Re-entrancy counter; `None` ⇒ 1 (the first cycle).
    pub cycle: Option<i32>,
    /// The phase: `"opened" | "won" | "lost" | "partial"`.
    pub phase: String,
    /// The funds-movement fact: `"withheld"` (card rails) | `"not_moved"`
    /// (invoice/ACH) — read at `opened` to choose the variant.
    pub funds_at_open: String,
    /// The disputed amount in minor units (`> 0`): the **gross** claim (what the
    /// buyer paid / the bank reverses), NOT net of the PSP fee — the ledger sizes a
    /// `CASH_HOLD` cash leg at `net = settled − fee` itself.
    pub disputed_amount_minor: i64,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// Phase instant; `None` ⇒ stamped at post time (current-month period).
    #[serde(default, with = "rfc3339::option")]
    pub effective_at: Option<OffsetDateTime>,
}

impl RecordDisputePhaseRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::RecordDisputePhase`],
    /// binding `dispute_id` from the request PATH (the body carries no dispute
    /// id — mirrors [`AllocatePaymentRequest::into_sdk`]) and validating the
    /// client-supplied id lengths at the boundary. `cycle` defaults to 1.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `dispute_id` (path), `payment_id`, or
    /// (when present) `invoice_id` is empty or exceeds [`MAX_BUSINESS_ID_LEN`], or
    /// when `cycle` is supplied and `< 1`.
    pub fn into_sdk(
        self,
        dispute_id: String,
    ) -> Result<bss_ledger_sdk::RecordDisputePhase, DomainError> {
        validate_business_id("dispute_id", &dispute_id)?;
        validate_business_id("payment_id", &self.payment_id)?;
        if let Some(invoice_id) = &self.invoice_id {
            validate_business_id("invoice_id", invoice_id)?;
        }
        check_currency_code("currency", &self.currency)?;
        // The DB CHECK (cycle >= 1) is authoritative; reject a non-positive cycle
        // at the boundary with a clear `InvalidRequest` rather than letting it hit
        // the constraint and surface as a generic 500.
        if let Some(cycle) = self.cycle
            && cycle < 1
        {
            return Err(DomainError::InvalidRequest(format!(
                "dispute cycle must be >= 1, got {cycle}"
            )));
        }
        Ok(bss_ledger_sdk::RecordDisputePhase {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            payment_id: self.payment_id,
            dispute_id,
            invoice_id: self.invoice_id,
            cycle: self.cycle.unwrap_or(1),
            phase: self.phase,
            funds_at_open: self.funds_at_open,
            disputed_amount_minor: self.disputed_amount_minor,
            currency: self.currency,
            scale: self.scale,
            effective_at: self.effective_at,
        })
    }
}

/// The `POST /disputes/{dispute_id}/phases` response when the phase POSTED
/// inline: a reference to the dispute phase posting. A fresh phase renders `201`,
/// an idempotent replay `200` (the handler reads `replayed`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecordDisputePhaseResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::DisputeRecorded> for RecordDisputePhaseResponse {
    fn from(r: bss_ledger_sdk::DisputeRecorded) -> Self {
        Self {
            entry_id: r.posting.entry_id,
            created_seq: r.posting.created_seq,
            replayed: r.posting.replayed,
        }
    }
}

/// The status token a queued (out-of-order) dispute phase renders in
/// [`DisputePhaseQueuedResponse::status`] — a kebab-case literal in a normal JSON
/// body, NOT a `problem+json` error: a `won`/`lost` whose `opened` has not landed
/// is accepted-for-later (HTTP 202), not rejected (§4.7 out-of-order).
const DISPUTE_PHASE_QUEUED_STATUS: &str = "dispute-phase-queued";

/// The `POST /disputes/{dispute_id}/phases` response when the phase was an
/// out-of-order `won`/`lost` whose `opened` has not landed: the request was
/// durably QUEUED for a later drain (HTTP 202 Accepted), not posted. `status` is
/// the fixed `dispute-phase-queued` token; `flow` + `business_id` are the queue
/// key and `queued_at` the intake instant. No posting handle — nothing has posted
/// yet. Mirrors `AllocationQueuedResponse`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DisputePhaseQueuedResponse {
    /// Always `dispute-phase-queued` (a normal-body kebab-case token, not an error).
    pub status: String,
    /// The deferred-apply queue flow (the `CHARGEBACK` literal).
    pub flow: String,
    /// The queue/dedup business id — `dispute_id:cycle:phase`.
    pub business_id: String,
    /// When the intake durably enqueued the request.
    #[serde(with = "rfc3339")]
    pub queued_at: OffsetDateTime,
}

impl From<bss_ledger_sdk::DisputeQueued> for DisputePhaseQueuedResponse {
    fn from(q: bss_ledger_sdk::DisputeQueued) -> Self {
        Self {
            status: DISPUTE_PHASE_QUEUED_STATUS.to_owned(),
            flow: q.flow,
            business_id: q.business_id,
            queued_at: q.queued_at,
        }
    }
}

/// One caller-computed allocation share (Mode B, §4.4 F-5): apply `amount_minor`
/// of the lump to `invoice_id`. A row of [`AllocatePaymentRequest::splits`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct AllocationSplitDto {
    pub invoice_id: String,
    pub amount_minor: i64,
}

impl From<AllocationSplitDto> for bss_ledger_sdk::AllocationSplit {
    fn from(s: AllocationSplitDto) -> Self {
        Self {
            invoice_id: s.invoice_id,
            amount_minor: s.amount_minor,
        }
    }
}

/// The `POST /payments/{payment_id}/allocations` request body: allocate a
/// settled payment's unallocated pool to the payer's open receivables (the
/// **money-out** side). The `payment_id` comes from the PATH (not the body —
/// see [`SettlePaymentRequest::into`] vs [`AllocatePaymentRequest::into_sdk`]),
/// so the lowering to the SDK type takes it as a parameter. `allocation_id` is
/// the idempotency key. `scale` is advisory.
///
/// When `splits` is omitted the `lump_minor` is applied by the tenant's
/// precedence policy and `hint_invoice_id` jumps one invoice to the front of
/// that fill order. When `splits` is supplied (Mode B), the precedence decision
/// is skipped and those explicit shares are validated against the open
/// receivables instead (each must name an open invoice and not over-allocate it;
/// the shares must sum to at most `lump_minor`) — a bad split is rejected
/// `ALLOCATION_SPLIT_INVALID`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct AllocatePaymentRequest {
    /// The seller tenant whose ledger this allocates within; the PEP gate
    /// target. Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// Idempotency key for the allocation (a re-issue replays).
    pub allocation_id: Uuid,
    /// The lump to apply in minor units, drained oldest-first across open AR.
    pub lump_minor: i64,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// Optional invoice to jump to the front of the oldest-first fill order.
    /// Ignored when `splits` is supplied (Mode B bypasses the fill order).
    pub hint_invoice_id: Option<String>,
    /// Optional caller-computed per-invoice split (Mode B escape hatch). `None`
    /// ⇒ the precedence policy decides the split.
    pub splits: Option<Vec<AllocationSplitDto>>,
}

impl AllocatePaymentRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::AllocatePayment`],
    /// binding `payment_id` from the request PATH (the body carries no payment
    /// id) and validating the client-supplied id lengths at the boundary.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `payment_id` (path), `hint_invoice_id`
    /// (when present), or any `splits[].invoice_id` is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`].
    pub fn into_sdk(
        self,
        payment_id: String,
    ) -> Result<bss_ledger_sdk::AllocatePayment, DomainError> {
        validate_business_id("payment_id", &payment_id)?;
        if let Some(hint) = &self.hint_invoice_id {
            validate_business_id("hint_invoice_id", hint)?;
        }
        if let Some(splits) = &self.splits {
            for split in splits {
                validate_business_id("splits.invoice_id", &split.invoice_id)?;
            }
        }
        check_currency_code("currency", &self.currency)?;
        Ok(bss_ledger_sdk::AllocatePayment {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            payment_id,
            allocation_id: self.allocation_id,
            lump_minor: self.lump_minor,
            currency: self.currency,
            scale: self.scale,
            hint_invoice_id: self.hint_invoice_id,
            splits: self.splits.map(|v| v.into_iter().map(Into::into).collect()),
        })
    }
}

/// One recorded allocation split in a response: how much of a payment was
/// applied to `invoice_id`, when, under which precedence policy. A row of
/// [`AllocatePaymentResponse::allocations`] and of the `GET
/// /payments/{payment_id}/allocations` list.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AllocationDto {
    pub invoice_id: String,
    pub amount_minor: i64,
    pub currency: String,
    #[serde(with = "rfc3339")]
    pub allocated_at_utc: OffsetDateTime,
    pub precedence_policy_ref: String,
}

impl From<bss_ledger_sdk::AllocationView> for AllocationDto {
    fn from(a: bss_ledger_sdk::AllocationView) -> Self {
        Self {
            invoice_id: a.invoice_id,
            amount_minor: a.amount_minor,
            currency: a.currency,
            allocated_at_utc: a.allocated_at_utc,
            precedence_policy_ref: a.precedence_policy_ref,
        }
    }
}

/// The `POST /payments/{payment_id}/allocations` response: the posting handle
/// plus the per-invoice splits the allocation applied (always `201`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AllocatePaymentResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
    pub allocations: Vec<AllocationDto>,
}

impl From<bss_ledger_sdk::AllocationApplied> for AllocatePaymentResponse {
    fn from(a: bss_ledger_sdk::AllocationApplied) -> Self {
        Self {
            entry_id: a.posting.entry_id,
            created_seq: a.posting.created_seq,
            replayed: a.posting.replayed,
            allocations: a.allocations.into_iter().map(AllocationDto::from).collect(),
        }
    }
}

/// The status token a queued (deferred) allocation renders in
/// [`AllocationQueuedResponse::status`] — a kebab-case literal in a normal JSON
/// body, NOT a `problem+json` error: an allocate of a not-yet-settled payment is
/// accepted-for-later (HTTP 202), not rejected (§4.7 allocation-before-settlement).
const ALLOCATION_QUEUED_STATUS: &str = "allocation-queued";

/// The `POST /payments/{payment_id}/allocations` response when the payment is not
/// yet settled: the allocation was durably QUEUED for a later drain (HTTP 202
/// Accepted), not posted. `status` is the fixed `allocation-queued` token; `flow`
/// + `business_id` are the queue key and `queued_at` the intake instant. No
/// posting handle / splits — nothing has posted yet (those arrive on the drain).
/// Mirrors the credit/settle status-varying rendering, but as a distinct body
/// shape since there is no `PostingRef` to surface.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AllocationQueuedResponse {
    /// Always `allocation-queued` (a normal-body kebab-case token, not an error).
    pub status: String,
    /// The deferred-apply queue flow (the `PAYMENT_ALLOCATE` literal).
    pub flow: String,
    /// The queue/dedup business id — the allocation's `allocation_id`.
    pub business_id: String,
    /// When the intake durably enqueued the request.
    #[serde(with = "rfc3339")]
    pub queued_at: OffsetDateTime,
}

impl From<bss_ledger_sdk::AllocationQueued> for AllocationQueuedResponse {
    fn from(q: bss_ledger_sdk::AllocationQueued) -> Self {
        Self {
            status: ALLOCATION_QUEUED_STATUS.to_owned(),
            flow: q.flow,
            business_id: q.business_id,
            queued_at: q.queued_at,
        }
    }
}

/// The `GET /payments/{payment_id}/allocations` response: the recorded splits
/// for a payment.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PaymentAllocationsDto {
    pub allocations: Vec<AllocationDto>,
}

impl From<Vec<bss_ledger_sdk::AllocationView>> for PaymentAllocationsDto {
    fn from(rows: Vec<bss_ledger_sdk::AllocationView>) -> Self {
        Self {
            allocations: rows.into_iter().map(AllocationDto::from).collect(),
        }
    }
}

/// The `GET /balances/unallocated` response: the payer's still-undrained pool
/// for a currency.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UnallocatedDto {
    pub payer_tenant_id: Uuid,
    pub currency: String,
    pub balance_minor: i64,
}

impl From<bss_ledger_sdk::UnallocatedView> for UnallocatedDto {
    fn from(u: bss_ledger_sdk::UnallocatedView) -> Self {
        Self {
            payer_tenant_id: u.payer_tenant_id,
            currency: u.currency,
            balance_minor: u.balance_minor,
        }
    }
}

// ── Credit-application request/response DTOs (§5.2, reusable-credit wallet) ───

/// Stamps `context.resource_type` on the 400s [`CreditApplicationRequest::into_sdk`]
/// raises for a malformed `kind` / missing field (mirrors the `resource_error`
/// structs in `error.rs` / `error_mapping.rs`). The body-shape validation lives
/// here on the DTO; the `DomainError` ladder handles the post-time caps.
#[resource_error(gts_id!("cf.bss.ledger.credit_application.v1~"))]
struct CreditApplicationResource;

/// The `POST /credit-applications` request body: ONE wire shape for both wallet
/// kinds, discriminated by `kind` (`"grant"` | `"apply"`). The target seller
/// ledger is the body's own `tenant_id` (tenant in body, not path — the vhp-core
/// REST convention); the `(credit_application, write)` PEP gate authorizes it.
/// `scale` is advisory; the ledger resolves the authoritative per-line scale.
///
/// The kind-specific fields are all optional on the wire and validated in
/// [`Self::into_sdk`]: a `grant` requires `amount_minor` + `credit_grant_event_type`
/// (and ignores `targets`); an `apply` requires a non-empty `targets` (and ignores
/// the grant fields). A missing required field or an unknown `kind` is rejected
/// `400 InvalidArgument` before the SDK type is built.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreditApplicationRequest {
    /// The wallet operation: `"grant"` (park pool cash) | `"apply"` (spend wallet).
    pub kind: String,
    /// The seller tenant whose ledger this posts into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    pub payer_tenant_id: Uuid,
    /// The `CREDIT_APPLY` idempotency business id (a replay returns the prior post).
    pub credit_application_id: String,
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// Grant only: amount to park into the wallet, in minor units. Required for
    /// `kind = "grant"`, ignored for `"apply"`.
    pub amount_minor: Option<i64>,
    /// Grant only: the wallet sub-grain bucket the credit accrues to. Required for
    /// `kind = "grant"`, ignored for `"apply"`.
    pub credit_grant_event_type: Option<String>,
    /// Apply only: the per-invoice receivable shares to spend the wallet against
    /// (validated against the payer's open AR). Required (non-empty) for
    /// `kind = "apply"`, ignored for `"grant"`.
    pub targets: Option<Vec<AllocationSplitDto>>,
}

impl CreditApplicationRequest {
    /// Lower the flat wire DTO into the SDK [`bss_ledger_sdk::CreditApplication`],
    /// dispatching on `kind` and validating the kind-specific fields at the
    /// boundary (a missing field / unknown kind is a `400 InvalidArgument`, never
    /// a panic or a silent default).
    ///
    /// # Errors
    /// A `400 InvalidArgument` [`CanonicalError`] when `credit_application_id` is
    /// empty / over [`MAX_BUSINESS_ID_LEN`], when `kind = "grant"` omits
    /// `amount_minor` / `credit_grant_event_type`, when `kind = "apply"` omits a
    /// non-empty `targets` (or a target names an empty / over-long invoice id), or
    /// when `kind` is neither `"grant"` nor `"apply"`.
    pub fn into_sdk(self) -> Result<bss_ledger_sdk::CreditApplication, CanonicalError> {
        if self.credit_application_id.is_empty()
            || self.credit_application_id.len() > MAX_BUSINESS_ID_LEN
        {
            return Err(invalid_field(
                "credit_application_id",
                format!("must be 1..={MAX_BUSINESS_ID_LEN} bytes"),
            ));
        }
        // Screen the currency at the boundary; the `DomainError` 400 flows through
        // the existing `From<DomainError> for CanonicalError` ladder so it renders
        // the same InvalidArgument shape as the kind-specific violations below.
        check_currency_code("currency", &self.currency).map_err(CanonicalError::from)?;
        match self.kind.as_str() {
            "grant" => {
                let amount_minor = self.amount_minor.ok_or_else(|| {
                    invalid_field("amount_minor", "grant requires `amount_minor`")
                })?;
                let credit_grant_event_type = self.credit_grant_event_type.ok_or_else(|| {
                    invalid_field(
                        "credit_grant_event_type",
                        "grant requires `credit_grant_event_type`",
                    )
                })?;
                Ok(bss_ledger_sdk::CreditApplication::Grant(
                    bss_ledger_sdk::CreditGrant {
                        tenant_id: self.tenant_id,
                        payer_tenant_id: self.payer_tenant_id,
                        credit_application_id: self.credit_application_id,
                        currency: self.currency,
                        scale: self.scale,
                        amount_minor,
                        credit_grant_event_type,
                    },
                ))
            }
            "apply" => {
                let targets = self.targets.filter(|t| !t.is_empty()).ok_or_else(|| {
                    invalid_field("targets", "apply requires a non-empty `targets`")
                })?;
                for t in &targets {
                    if t.invoice_id.is_empty() || t.invoice_id.len() > MAX_BUSINESS_ID_LEN {
                        return Err(invalid_field(
                            "targets.invoice_id",
                            format!("must be 1..={MAX_BUSINESS_ID_LEN} bytes"),
                        ));
                    }
                }
                Ok(bss_ledger_sdk::CreditApplication::Apply(
                    bss_ledger_sdk::CreditApply {
                        tenant_id: self.tenant_id,
                        payer_tenant_id: self.payer_tenant_id,
                        credit_application_id: self.credit_application_id,
                        currency: self.currency,
                        scale: self.scale,
                        targets: targets.into_iter().map(Into::into).collect(),
                    },
                ))
            }
            other => Err(invalid_field(
                "kind",
                format!(
                    "unknown credit application kind {other:?} (expected \"grant\" or \"apply\")"
                ),
            )),
        }
    }
}

/// Build a `400 InvalidArgument` [`CanonicalError`] carrying one `field`
/// violation for a malformed credit-application body (a missing kind-specific
/// field or an unknown `kind`).
fn invalid_field(field: &'static str, message: impl Into<String>) -> CanonicalError {
    CreditApplicationResource::invalid_argument()
        .with_field_violation(field, message.into(), "INVALID_CREDIT_APPLICATION")
        .create()
}

/// One per-sub-grain wallet draw-down in a credit-application response: how much
/// the apply drew from the `credit_grant_event_type` bucket. A row of
/// [`CreditApplicationResponse::debits`] (empty for a grant).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CreditDebitDto {
    pub credit_grant_event_type: String,
    pub amount_minor: i64,
}

impl From<bss_ledger_sdk::CreditDebitView> for CreditDebitDto {
    fn from(d: bss_ledger_sdk::CreditDebitView) -> Self {
        Self {
            credit_grant_event_type: d.credit_grant_event_type,
            amount_minor: d.amount_minor,
        }
    }
}

/// One per-invoice receivable share in a credit-application apply response: how
/// much of the wallet was applied to `invoice_id`. A row of
/// [`CreditApplicationResponse::applications`] (empty for a grant). Mirrors the
/// SDK's reuse of `AllocationSplit` on the apply side.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CreditApplicationShareDto {
    pub invoice_id: String,
    pub amount_minor: i64,
}

impl From<bss_ledger_sdk::AllocationSplit> for CreditApplicationShareDto {
    fn from(s: bss_ledger_sdk::AllocationSplit) -> Self {
        Self {
            invoice_id: s.invoice_id,
            amount_minor: s.amount_minor,
        }
    }
}

/// The `POST /credit-applications` response: the posting handle plus — for an
/// apply — the per-sub-grain wallet draw-downs (`debits`) and the per-invoice
/// receivable shares (`applications`). A grant leaves both vecs empty (it moves
/// no wallet/AR splits). Always `201` (the wallet post is never an idempotent
/// `200`-replay at the handler — a replay returns the prior `posting` with
/// `replayed = true`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CreditApplicationResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
    pub debits: Vec<CreditDebitDto>,
    pub applications: Vec<CreditApplicationShareDto>,
}

impl From<bss_ledger_sdk::CreditApplicationApplied> for CreditApplicationResponse {
    fn from(a: bss_ledger_sdk::CreditApplicationApplied) -> Self {
        Self {
            entry_id: a.posting.entry_id,
            created_seq: a.posting.created_seq,
            replayed: a.posting.replayed,
            debits: a.debits.into_iter().map(CreditDebitDto::from).collect(),
            applications: a
                .applications
                .into_iter()
                .map(CreditApplicationShareDto::from)
                .collect(),
        }
    }
}

// ── Recognition-run request/response DTOs (§5, the ASC 606 S6 release) ───────

/// The `POST /recognition-runs` request body: trigger an ASC 606 recognition
/// run for one fiscal period (release the period's due `PENDING` segments). The
/// target seller ledger is the body's own `tenant_id` (tenant in body, not path
/// — the vhp-core REST convention); the `(entry, post)` PEP gate authorizes it
/// (a run posts `DR CL / CR Revenue` journal entries). `run_id` is the
/// run-trigger idempotency key: `None` ⇒ the ledger mints a fresh one (a first,
/// un-keyed trigger); a stable one makes retries idempotent (a replay returns
/// the prior run reference without starting a second run).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[allow(
    clippy::struct_field_names,
    reason = "the *_id fields mirror the run-trigger identity tuple (tenant / period / run)"
)]
pub struct TriggerRecognitionRunRequest {
    /// The seller tenant whose ledger this releases revenue in; the PEP gate
    /// target. Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The fiscal period to release due segments for (`YYYYMM`).
    pub period_id: String,
    /// The run-trigger idempotency key; `None` ⇒ the ledger mints a fresh one.
    pub run_id: Option<Uuid>,
}

impl TriggerRecognitionRunRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::TriggerRecognitionRun`],
    /// validating the `period_id` at the boundary (a `YYYYMM` business id, bound
    /// by the same `varchar(128)` column convention as the other ids).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `period_id` is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`].
    pub fn into_sdk(self) -> Result<bss_ledger_sdk::TriggerRecognitionRun, DomainError> {
        validate_business_id("period_id", &self.period_id)?;
        Ok(bss_ledger_sdk::TriggerRecognitionRun {
            tenant_id: self.tenant_id,
            period_id: self.period_id,
            run_id: self.run_id,
        })
    }
}

/// The `POST /recognition-runs` response when the run EXECUTED (fresh or an
/// idempotent replay of a prior trigger): the run identity + the release tally.
/// A fresh run renders `200`, an idempotent replay also `200` (the run already
/// committed under the original trigger; `replayed = true`). The `Ran` arm of
/// the outcome.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecognitionRunResponse {
    /// The run that executed (minted by the trigger, or replayed).
    pub run_id: Uuid,
    /// The period the run released for (`YYYYMM`).
    pub period_id: String,
    /// `true` when this trigger replayed a prior run with the same
    /// `(tenant, run_id)` (no new run row was inserted; no re-release).
    pub replayed: bool,
    /// Segments released on THIS pass (a fresh `DR CL / CR Revenue` post).
    pub released: usize,
    /// Segments that were already released (an idempotent `RECOGNITION` replay).
    pub already_recognized: usize,
}

impl From<bss_ledger_sdk::RecognitionRunRef> for RecognitionRunResponse {
    fn from(r: bss_ledger_sdk::RecognitionRunRef) -> Self {
        Self {
            run_id: r.run_id,
            period_id: r.period_id,
            replayed: r.replayed,
            released: r.released,
            already_recognized: r.already_recognized,
        }
    }
}

/// The status token a recognition run that parked out-of-order segments renders
/// in [`RecognitionRunQueuedResponse::status`] — a kebab-case literal in a normal
/// JSON body, NOT a `problem+json` error: a due segment whose lower-period
/// predecessor is not yet `DONE` is accepted-for-later (HTTP 202), not rejected
/// (§4.6 ordering, uniform with `allocation-queued` / `dispute-phase-queued`).
const RECOGNITION_PERIOD_QUEUED_STATUS: &str = "recognition-period-queued";

/// The `POST /recognition-runs` response when the run had to park one or more
/// out-of-order segments `QUEUED` for a later drain (HTTP 202 Accepted): the run
/// ran (it may have released in-order segments) but a due segment's lower-period
/// predecessor was not yet `DONE` (§4.6). `status` is the fixed
/// `recognition-period-queued` token; `released` + `queued` are this pass's
/// tallies. A later run drains the `QUEUED` segments once their predecessors
/// commit. Mirrors `AllocationQueuedResponse`. The `Queued` arm of the outcome.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecognitionRunQueuedResponse {
    /// Always `recognition-period-queued` (a normal-body kebab-case token, not an error).
    pub status: String,
    /// The run that executed (the queued ones are the out-of-order tail).
    pub run_id: Uuid,
    /// The period the run was triggered for (`YYYYMM`).
    pub period_id: String,
    /// Segments released in order on this pass before/around the queued ones.
    pub released: usize,
    /// Segments parked `QUEUED` this pass (a predecessor period was not `DONE`).
    pub queued: usize,
}

impl From<bss_ledger_sdk::RecognitionRunQueued> for RecognitionRunQueuedResponse {
    fn from(q: bss_ledger_sdk::RecognitionRunQueued) -> Self {
        Self {
            status: RECOGNITION_PERIOD_QUEUED_STATUS.to_owned(),
            run_id: q.run_id,
            period_id: q.period_id,
            released: q.released,
            queued: q.queued,
        }
    }
}

// ── Revenue-disaggregation response DTOs (§3.5 / §4.5, ASC 606 by stream) ─────

/// One disaggregated recognized-revenue grain in the `GET
/// /revenue/disaggregation` response: the revenue RECOGNIZED into
/// `revenue_stream` during `period_id` (`Σ amount_minor` of the DONE recognition
/// segments at that grain), in minor units of `currency`. A row of
/// [`RevenueDisaggregationResponse::entries`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RevenueDisaggregationEntryDto {
    /// The fiscal period the revenue recognized in (`YYYYMM`).
    pub period_id: String,
    /// The revenue stream the recognized revenue books to.
    pub revenue_stream: String,
    /// Revenue recognized into this `(period, stream)` grain, in minor units.
    pub recognized_minor: i64,
    /// ISO currency of the recognized amount.
    pub currency: String,
}

impl From<bss_ledger_sdk::RevenueDisaggregationEntry> for RevenueDisaggregationEntryDto {
    fn from(e: bss_ledger_sdk::RevenueDisaggregationEntry) -> Self {
        Self {
            period_id: e.period_id,
            revenue_stream: e.revenue_stream,
            recognized_minor: e.recognized_minor,
            currency: e.currency,
        }
    }
}

/// The `GET /revenue/disaggregation` response: recognized ASC 606 revenue
/// disaggregated by `(period_id, revenue_stream)`, ordered by
/// `(period_id, revenue_stream)`. NOTE: like `ar-aging`, this is a **computed
/// aggregate report** (a grouped SUM over the recognized segments), not a
/// paginated row collection — it keeps plain `?tenant_id=&period_id=` query
/// params (no `OData` `$filter` / `Page` envelope).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RevenueDisaggregationResponse {
    pub entries: Vec<RevenueDisaggregationEntryDto>,
}

impl From<bss_ledger_sdk::RevenueDisaggregation> for RevenueDisaggregationResponse {
    fn from(d: bss_ledger_sdk::RevenueDisaggregation) -> Self {
        Self {
            entries: d
                .entries
                .into_iter()
                .map(RevenueDisaggregationEntryDto::from)
                .collect(),
        }
    }
}

// ── Schedule change/cancel request/response DTOs (§3.6 / §4.6, Group H) ───────

/// One replacement recognition segment on a `replace` change (Group H). Maps to
/// the SDK [`bss_ledger_sdk::ChangeSegment`]; ignored on a `cancel`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ChangeSegmentDto {
    /// Fiscal `period_id` (`YYYYMM`) this replacement segment recognizes into.
    pub period_id: String,
    /// Minor-unit amount of this segment (`>= 0`).
    pub amount_minor: i64,
}

impl From<ChangeSegmentDto> for bss_ledger_sdk::ChangeSegment {
    fn from(s: ChangeSegmentDto) -> Self {
        Self {
            period_id: s.period_id,
            amount_minor: s.amount_minor,
        }
    }
}

/// The `POST /recognition-schedules/{schedule_id}/changes` request body: change or
/// cancel an ASC 606 recognition schedule (design §3.6 / §4.6). `{schedule_id}`
/// (the ACTIVE schedule to change) comes from the PATH; the target seller ledger
/// is the body's own `tenant_id` (tenant in body, not path — the vhp-core REST
/// convention); the `(entry, post)` PEP gate authorizes it (a change marks/mints
/// schedule state). `change_id` is the idempotency key (a replay returns the prior
/// result, minting no second schedule). `action` is `"cancel"` | `"replace"`.
/// `treatment` is the upstream modification-accounting decision: `"prospective"` /
/// `"separate_contract"` apply; `"catch_up"` / unknown is rejected
/// `MODIFICATION_TREATMENT_REVIEW` (the ledger never silently treats a
/// modification as prospective). `new_segments` is required for a `replace` (the
/// NEW version's plan of the remaining deferred), ignored for a `cancel`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ChangeRecognitionScheduleRequest {
    /// The seller tenant whose schedule this changes; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The change idempotency key — a replay returns the prior result.
    pub change_id: String,
    /// The change action: `"cancel"` | `"replace"`.
    pub action: String,
    /// The upstream modification treatment: `"prospective"` | `"separate_contract"`
    /// (apply) | `"catch_up"` / unknown (review).
    pub treatment: String,
    /// The replacement segments for a `replace` (the new version's plan of the
    /// remaining deferred); omit for a `cancel`.
    pub new_segments: Option<Vec<ChangeSegmentDto>>,
}

impl ChangeRecognitionScheduleRequest {
    /// Lower the wire DTO into the SDK [`bss_ledger_sdk::ChangeRecognitionSchedule`],
    /// binding `schedule_id` from the request PATH (the body carries no schedule
    /// id — mirrors [`AllocatePaymentRequest::into_sdk`]) and validating the
    /// client-supplied id lengths at the boundary. The `action` / `treatment`
    /// literals are parsed deeper (in the change service), so a bad value surfaces
    /// as `MODIFICATION_TREATMENT_REVIEW` / an unknown-action `InvalidArgument`
    /// from there — not here.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `schedule_id` (path) or `change_id`
    /// (body) is empty or exceeds [`MAX_BUSINESS_ID_LEN`].
    pub fn into_sdk(
        self,
        schedule_id: String,
    ) -> Result<bss_ledger_sdk::ChangeRecognitionSchedule, DomainError> {
        validate_business_id("schedule_id", &schedule_id)?;
        validate_business_id("change_id", &self.change_id)?;
        Ok(bss_ledger_sdk::ChangeRecognitionSchedule {
            tenant_id: self.tenant_id,
            schedule_id,
            change_id: self.change_id,
            action: self.action,
            treatment: self.treatment,
            new_segments: self
                .new_segments
                .map(|v| v.into_iter().map(Into::into).collect()),
        })
    }
}

/// The `POST /recognition-schedules/{schedule_id}/changes` response: a small
/// reference to the change's outcome. `new_schedule_id` is the successor version's
/// id on a `replace` (`null` on a `cancel`); `status` is the original schedule's
/// resulting status (`"REPLACED"` | `"CANCELLED"`). Always `200` (the change is
/// applied or an idempotent replay).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ScheduleChangeResponse {
    /// The original schedule that was cancelled / replaced.
    pub schedule_id: String,
    /// The successor schedule version's id on a `replace`; `null` on a `cancel`.
    pub new_schedule_id: Option<String>,
    /// The original schedule's resulting status (`"REPLACED"` | `"CANCELLED"`).
    pub status: String,
}

impl From<bss_ledger_sdk::ScheduleChangeRef> for ScheduleChangeResponse {
    fn from(r: bss_ledger_sdk::ScheduleChangeRef) -> Self {
        Self {
            schedule_id: r.schedule_id,
            new_schedule_id: r.new_schedule_id,
            status: r.status,
        }
    }
}

// ── Recognition-schedule read DTOs (§3.7 / §4, GET /recognition-schedules/{id}) ─

/// One recognition segment in the `GET /recognition-schedules/{schedule_id}`
/// response: the `segment_no` (immutable, 1:1 with `period_id`), the period it
/// recognizes into, its minor-unit amount, and its release status
/// (`PENDING` | `QUEUED` | `DONE`). A row of [`RecognitionScheduleResponse::segments`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecognitionScheduleSegmentDto {
    /// The immutable segment number (1:1 with `period_id`).
    pub segment_no: i32,
    /// The fiscal period this segment recognizes into (`YYYYMM`).
    pub period_id: String,
    /// The segment's minor-unit amount.
    pub amount_minor: i64,
    /// The release status (`PENDING` | `QUEUED` | `DONE`).
    pub status: String,
}

impl From<bss_ledger_sdk::RecognitionScheduleSegmentView> for RecognitionScheduleSegmentDto {
    fn from(s: bss_ledger_sdk::RecognitionScheduleSegmentView) -> Self {
        Self {
            segment_no: s.segment_no,
            period_id: s.period_id,
            amount_minor: s.amount_minor,
            status: s.status,
        }
    }
}

/// The `GET /recognition-schedules/{schedule_id}` response: one schedule's
/// lifecycle view (design §3.7 / §4) — the schedule header (status, version,
/// revenue stream, currency, total-deferred / recognized-to-date, the originating
/// invoice + the §4.7 item-link anchor, the PO / subscription / policy refs) plus
/// its segments, ordered by `segment_no` (period order). A schedule outside the
/// caller's authorized subtree (or simply absent) yields a 404 — never this body
/// (no existence leak).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
// The `*_ref` / `*_id` fields mirror the `recognition_schedule` columns verbatim
// (the storage/SDK contract); renaming to satisfy `struct_field_names` would
// diverge from `RecognitionScheduleView`.
#[allow(clippy::struct_field_names)]
pub struct RecognitionScheduleResponse {
    /// The schedule's business id.
    pub schedule_id: String,
    /// The durable lifecycle status (`ACTIVE` | `REPLACED` | `CANCELLED` | …).
    pub status: String,
    /// The lineage version (`0` fresh; `old + 1` for a `replace` successor).
    pub version: i64,
    /// The revenue stream the obligation books to (one schedule per stream).
    pub revenue_stream: String,
    /// ISO-4217 currency (one schedule/account per currency).
    pub currency: String,
    /// The total deferred Contract-liability the schedule plans to release.
    pub total_deferred_minor: i64,
    /// The cumulative recognized-to-date (`<= total_deferred_minor`).
    pub recognized_minor: i64,
    /// The originating posted invoice.
    pub source_invoice_id: String,
    /// The Contract-liability invoice line the schedule draws down (the §4.7
    /// invoice-link anchor).
    pub source_invoice_item_ref: String,
    /// The PO / allocation group this obligation books under; `null` when none.
    pub po_allocation_group: Option<String>,
    /// The subscription / entitlement this obligation belongs to; `null` when none.
    pub subscription_ref: Option<String>,
    /// The immutable deferral/timing policy version stamped at build.
    pub policy_ref: String,
    /// The schedule's segments, ordered by `segment_no` (period order).
    pub segments: Vec<RecognitionScheduleSegmentDto>,
}

impl From<bss_ledger_sdk::RecognitionScheduleView> for RecognitionScheduleResponse {
    fn from(v: bss_ledger_sdk::RecognitionScheduleView) -> Self {
        Self {
            schedule_id: v.schedule_id,
            status: v.status,
            version: v.version,
            revenue_stream: v.revenue_stream,
            currency: v.currency,
            total_deferred_minor: v.total_deferred_minor,
            recognized_minor: v.recognized_minor,
            source_invoice_id: v.source_invoice_id,
            source_invoice_item_ref: v.source_invoice_item_ref,
            po_allocation_group: v.po_allocation_group,
            subscription_ref: v.subscription_ref,
            policy_ref: v.policy_ref,
            segments: v
                .segments
                .into_iter()
                .map(RecognitionScheduleSegmentDto::from)
                .collect(),
        }
    }
}

/// One recognition schedule header in the `GET /recognition-schedules` list
/// response — like [`RecognitionScheduleResponse`] but WITHOUT segments (the
/// by-id `GET /recognition-schedules/{schedule_id}` carries those). Maps from
/// [`bss_ledger_sdk::RecognitionScheduleSummaryView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct RecognitionScheduleSummaryDto {
    /// The schedule's business id.
    pub schedule_id: String,
    /// The durable lifecycle status (`ACTIVE` | `REPLACED` | `CANCELLED` | …).
    pub status: String,
    /// The lineage version (`0` fresh; `old + 1` for a `replace` successor).
    pub version: i64,
    /// The revenue stream the obligation books to (one schedule per stream).
    pub revenue_stream: String,
    /// ISO-4217 currency (one schedule/account per currency).
    pub currency: String,
    /// The total deferred Contract-liability the schedule plans to release.
    pub total_deferred_minor: i64,
    /// The cumulative recognized-to-date (`<= total_deferred_minor`).
    pub recognized_minor: i64,
    /// The originating posted invoice.
    pub source_invoice_id: String,
    /// The Contract-liability invoice line the schedule draws down.
    pub source_invoice_item_ref: String,
    /// The PO / allocation group this obligation books under; `null` when none.
    pub po_allocation_group: Option<String>,
    /// The subscription / entitlement this obligation belongs to; `null` when none.
    pub subscription_ref: Option<String>,
    /// The immutable deferral/timing policy version stamped at build.
    pub policy_ref: String,
}

impl From<bss_ledger_sdk::RecognitionScheduleSummaryView> for RecognitionScheduleSummaryDto {
    fn from(v: bss_ledger_sdk::RecognitionScheduleSummaryView) -> Self {
        Self {
            schedule_id: v.schedule_id,
            status: v.status,
            version: v.version,
            revenue_stream: v.revenue_stream,
            currency: v.currency,
            total_deferred_minor: v.total_deferred_minor,
            recognized_minor: v.recognized_minor,
            source_invoice_id: v.source_invoice_id,
            source_invoice_item_ref: v.source_invoice_item_ref,
            po_allocation_group: v.po_allocation_group,
            subscription_ref: v.subscription_ref,
            policy_ref: v.policy_ref,
        }
    }
}

/// The `GET /recognition-schedules` response: the recognition schedule headers
/// matching the `(tenant_id[, invoice_id][, revenue_stream])` filter (segments
/// omitted — the by-id surface carries those). A discovery surface for the
/// server-minted `schedule_id`; an empty list is a normal `200`, never a `404`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecognitionScheduleListResponse {
    /// The matching schedule headers (`0..=cap`).
    pub schedules: Vec<RecognitionScheduleSummaryDto>,
    /// `true` when the result was capped server-side — more schedules exist than
    /// returned (the list surface is not paginated). A client seeing `truncated`
    /// must narrow the filter (`invoice_id` / `revenue_stream`) to see the rest.
    pub truncated: bool,
}

// ── PII erasure / re-identification request + response DTOs (§4.5, Group 3A) ──

/// `POST …/audit/erasure` request body: the payer to erase. The free-text
/// investigation reason is the **`X-Investigation-Reason` header** (§5 — the
/// single source for the reason recorded on the `erasure` record), not a body
/// field. `target_scope` opens a DIFFERENT tenant's PII map (the cross-tenant
/// DPO path, §5) — absent or the caller's own ⇒ routine same-tenant erasure; a
/// different tenant is forensic-gated (authorized for `(entry, erase)` on the
/// target + a required reason, else `CROSS_TENANT_ACCESS_DENIED` /
/// `MISSING_INVESTIGATION_REASON`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ErasureRequestDto {
    pub payer_tenant_id: Uuid,
    /// The tenant whose PII map to erase (defaults to the caller's own).
    pub target_scope: Option<Uuid>,
}

/// `POST …/audit/reidentify` request body: the payer to re-identify + the
/// machine `reason_code`. The free-text `reason` is the **`X-Investigation-Reason`
/// header** (§5), not a body field; both reason and `reason_code` are required
/// (a missing one is rejected `MISSING_INVESTIGATION_REASON`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ReidentifyRequestDto {
    pub payer_tenant_id: Uuid,
    pub reason_code: String,
    /// The tenant whose PII map to re-identify against (defaults to the caller's
    /// own). A different tenant is the forensic cross-tenant path (§5):
    /// authorized for `(entry, reidentify)` on the target, else
    /// `CROSS_TENANT_ACCESS_DENIED`.
    pub target_scope: Option<Uuid>,
}

impl ReidentifyRequestDto {
    /// Cap the machine `reason_code` at the boundary (this DTO has no lowering
    /// method — the handler calls this before the re-identify).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `reason_code` exceeds
    /// [`MAX_REASON_CODE_LEN`].
    pub(crate) fn validate(&self) -> Result<(), DomainError> {
        check_free_text("reason_code", &self.reason_code, MAX_REASON_CODE_LEN)
    }
}

/// `POST …/audit/reidentify` response: the recovered opaque `pii_ref` (the
/// pointer into the external PII store; never the PII itself).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReidentifyResponseDto {
    pub pii_ref: String,
}

// ── Credit-note / debit-note / exposure DTOs (Slice 3 §4.2 / §4.3 / §4.7, Group E) ─
//
// Wire shape: `snake_case` + the gear's flat money triple (`amount_minor` +
// `currency` + advisory `scale`), the SAME convention every other ledger DTO
// uses (the `api_dto` macro fixes `snake_case`, see the module header) — NOT the
// design doc's illustrative `camelCase` / nested `{amountMinor,currency,scale}`
// money object (that overrides nothing here; the dto.rs convention is
// authoritative and dylint-enforced, exactly as the payment / dispute / credit
// surfaces already ship). `scale` is advisory on the request: the ledger resolves
// the authoritative per-line scale from the provisioned currency config (mirrors
// `SettlePaymentRequest`); the credit/debit-note domain requests carry no scale.

/// The `POST /credit-notes` request body: a compensating credit note against a
/// posted invoice (design §4.2). The target seller ledger is the body's own
/// `tenant_id` (tenant in body, not path — the vhp-core REST convention); the
/// `(entry, post)` PEP gate authorizes it (a credit note posts a balanced
/// compensating journal entry — the same data-plane post action as the
/// recognition run / invoice post). Idempotent on `credit_note_id` (the
/// `(tenant, CREDIT_NOTE, credit_note_id)` engine claim): a replay returns the
/// prior posting with `replayed = true`.
///
/// `amount_minor` is **incl-tax**; `tax_minor` is the reversed-tax slice of it;
/// `requested_deferred_minor` is the **split intent** — how much of the ex-tax
/// revenue portion (`amount_minor − tax_minor`) targets the unreleased deferred
/// balance (the rest reduces recognized revenue). `goodwill = true` ⇒ an AR-only
/// goodwill credit (debits `GOODWILL`, touches no schedule, MUST carry
/// `requested_deferred_minor = 0`). A malformed shape (negative amounts, tax over
/// amount, deferred over ex-tax, empty reason, goodwill-with-deferred) is rejected
/// `400` (`AMOUNT_OUT_OF_RANGE` / `InvalidArgument`) by the domain `validate_shape`;
/// an indeterminable split is `CREDIT_NOTE_SPLIT_AMBIGUOUS`; an over-headroom note
/// is `CREDIT_NOTE_EXCEEDS_HEADROOM`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
// The `*_id` / `*_ref` fields mirror the domain `CreditNoteRequest` / storage
// column names verbatim; renaming to satisfy `struct_field_names` would diverge
// from the contract.
#[allow(clippy::struct_field_names)]
pub struct CreditNoteRequest {
    /// The seller tenant whose ledger this posts into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The tenant the original invoice billed (the AR / wallet owner).
    pub payer_tenant_id: Uuid,
    /// The `(tenant, CREDIT_NOTE, credit_note_id)` idempotency business id + the
    /// `credit_note` row PK.
    pub credit_note_id: String,
    /// The originating posted invoice the note credits (its rows are never
    /// mutated).
    pub origin_invoice_id: String,
    /// The targeted posted invoice-item ref (the line being credited); `None` for
    /// an invoice-level credit.
    pub origin_invoice_item_ref: Option<String>,
    /// The PO / allocation group the targeted line books under (the split-basis
    /// dimension); `None` for a line with no group.
    pub po_allocation_group: Option<String>,
    /// The revenue stream the credit books against (per-stream legs carry it).
    pub revenue_stream: String,
    /// ISO-4217 currency of the note (all legs share it).
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// The note amount **incl-tax**, in minor units (`>= 0`, `> 0` enforced).
    pub amount_minor: i64,
    /// The tax slice of `amount_minor` to reverse onto `TAX_PAYABLE` (`>= 0`,
    /// `<= amount_minor`).
    pub tax_minor: i64,
    /// The authoritative tax breakdown — one component per `(jurisdiction,
    /// filing-period, rate)`, each reversing onto its OWN `TAX_PAYABLE` leg so
    /// `tax_subbalance` disaggregates (§4.5). Empty ⇒ a single dimensionless tax leg
    /// from `tax_minor` (legacy). A non-empty breakdown MUST sum to `tax_minor` (a
    /// `400` otherwise); `tax_minor` stays the split scalar.
    pub tax: Vec<TaxBreakdownDto>,
    /// The split **intent**: how much of the ex-tax revenue amount targets the
    /// unreleased deferred balance (`0 <= … <= amount_minor − tax_minor`). MUST be
    /// `0` when `goodwill` is set.
    pub requested_deferred_minor: i64,
    /// The mandatory business reason code (AC #14) recorded on the `credit_note`
    /// row.
    pub reason_code: String,
    /// `true` ⇒ an AR-only goodwill credit (`GOODWILL`, no schedule reduction);
    /// `None` ⇒ `false`.
    pub goodwill: Option<bool>,
}

impl CreditNoteRequest {
    /// Lower the wire DTO into the domain
    /// [`crate::domain::adjustment::credit_note::CreditNoteRequest`], validating
    /// the client-supplied id lengths at the boundary (the amounts / goodwill shape
    /// are validated by the domain `validate_shape` in the handler — a `400` from
    /// there). The advisory `scale` is dropped (the handler resolves the
    /// authoritative scale). `goodwill` defaults to `false`.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `credit_note_id` / `origin_invoice_id`
    /// (or, when present, `origin_invoice_item_ref`) is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`].
    pub fn into_domain(
        self,
    ) -> Result<crate::domain::adjustment::credit_note::CreditNoteRequest, DomainError> {
        validate_business_id("credit_note_id", &self.credit_note_id)?;
        validate_business_id("origin_invoice_id", &self.origin_invoice_id)?;
        if let Some(item) = &self.origin_invoice_item_ref {
            validate_business_id("origin_invoice_item_ref", item)?;
        }
        check_currency_code("currency", &self.currency)?;
        check_free_text("reason_code", &self.reason_code, MAX_REASON_CODE_LEN)?;
        Ok(crate::domain::adjustment::credit_note::CreditNoteRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            credit_note_id: self.credit_note_id,
            origin_invoice_id: self.origin_invoice_id,
            origin_invoice_item_ref: self.origin_invoice_item_ref,
            po_allocation_group: self.po_allocation_group,
            revenue_stream: self.revenue_stream,
            currency: self.currency,
            amount_minor: self.amount_minor,
            tax_minor: self.tax_minor,
            tax: self.tax.into_iter().map(TaxBreakdown::from).collect(),
            requested_deferred_minor: self.requested_deferred_minor,
            reason_code: self.reason_code,
            goodwill: self.goodwill.unwrap_or(false),
        })
    }
}

/// The `POST /credit-notes` response: a reference to the compensating-entry
/// posting. A fresh post renders `201`, an idempotent replay `200` (the handler
/// reads `replayed`). Mirrors [`PostingRefDto`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CreditNoteResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for CreditNoteResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// One planned leg of a governed manual adjustment (design §4.6). `account_class`
/// + `side` carry the stable wire literals (parsed at [`ManualAdjustmentRequest::into_domain`]):
/// the class is an [`AccountClass`] token (e.g. `"SUSPENSE"`, `"CASH_CLEARING"`,
/// `"AR"`, `"GOODWILL"`); the side is `"DR"` / `"CR"` ([`Side`]). A leg outside the
/// action's code-owned allow-list — or any `REVENUE` / `CONTRACT_LIABILITY` leg, or
/// an unpaired `CONTRA_REVENUE` write-off — is rejected by the domain `govern` gate
/// (`400 MANUAL_ADJUSTMENT_NOT_ALLOWED`). `revenue_stream` is carried only for a
/// per-stream class and is otherwise `null`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ManualLegDto {
    /// The [`AccountClass`] wire token this leg posts to (parsed in `into_domain`).
    pub account_class: String,
    /// The [`Side`] wire token: `"DR"` (debit) / `"CR"` (credit).
    pub side: String,
    /// The leg amount in minor units (`> 0`; the domain `govern` rejects
    /// zero/negative legs).
    pub amount_minor: i64,
    /// The revenue stream — `Some` only for a per-stream class; `null` otherwise.
    pub revenue_stream: Option<String>,
}

/// The `POST /manual-adjustments` request body: a *governed* manual adjustment
/// (design §4.6) — the ledger's escape hatch for corrections the typed flows
/// (invoice / settle / allocate / S3 notes / S4 recognition) do not cover (rounding
/// residue, suspense / cash-clearing clean-up). The target seller ledger is the
/// body's own `tenant_id` (tenant in body, not path); the `(entry, post)` PEP gate
/// authorizes it. Idempotent on `adjustment_id` (the `(tenant, MANUAL_ADJUSTMENT,
/// adjustment_id)` engine claim): a replay returns the prior posting with `replayed
/// = true`.
///
/// `action` selects a code-owned allow-list of account classes the legs may touch;
/// `REVENUE` / `CONTRACT_LIABILITY` are globally off-limits and an unpaired
/// `CONTRA_REVENUE` leg is rejected as an attempted write-off (all `400
/// MANUAL_ADJUSTMENT_NOT_ALLOWED`). A `reason_code` is mandatory (AC #14). The
/// preparer actor is the AUTHENTICATED subject (stamped server-side, never read from
/// the body); the approver is assigned by the dual-control approval flow. A
/// governed adjustment whose gross (`Σ DR`) crosses the tenant's D2 threshold routes
/// to dual-control (`409 DUAL_CONTROL_REQUIRED`) instead of posting inline.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct ManualAdjustmentRequest {
    /// The seller tenant whose ledger this posts into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The payer tenant the legs attribute to when the adjustment touches a
    /// payer-scoped balance (`AR` / `UNALLOCATED`); `null` for a payer-less internal
    /// clean-up.
    pub payer_tenant_id: Option<Uuid>,
    /// The `(tenant, MANUAL_ADJUSTMENT, adjustment_id)` idempotency business id.
    pub adjustment_id: String,
    /// The governed action wire token (`"ROUNDING_CORRECTION"` /
    /// `"SUSPENSE_CLEAR"`) — selects the allow-list, parsed in `into_domain`.
    pub action: String,
    /// ISO-4217 currency of the adjustment (every leg shares it).
    pub currency: String,
    /// The legs to post — must net to zero (`Σ DR == Σ CR`, enforced by `govern`).
    pub legs: Vec<ManualLegDto>,
    /// The mandatory business reason code (AC #14); an empty/blank one is rejected.
    pub reason_code: String,
    /// The authoritative tax breakdown for a tax-bearing action (never recomputed);
    /// usually empty for the MVP actions.
    pub tax: Vec<TaxBreakdownDto>,
}

impl ManualAdjustmentRequest {
    /// Lower the wire DTO into the domain
    /// [`crate::domain::adjustment::manual::ManualAdjustmentRequest`], parsing the
    /// action + each leg's `account_class` / `side` literals at the boundary (a bad
    /// literal is a clean `400`, not a deep failure). `preparer_actor_id` is the
    /// AUTHENTICATED subject (passed in by the handler from `ctx.subject_id()`),
    /// never trusted from the body; `approver_actor_id` is `None` on the POST (it is
    /// assigned by the dual-control approval flow). The amounts / balance / allow-list
    /// are validated by the domain `govern` gate in the handler.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `adjustment_id` is empty / over
    /// [`MAX_BUSINESS_ID_LEN`], when `action` is not a known
    /// [`crate::domain::adjustment::manual::ManualAdjustmentAction`], or when a leg's
    /// `account_class` / `side` literal does not parse.
    pub fn into_domain(
        self,
        preparer_actor_id: Uuid,
    ) -> Result<crate::domain::adjustment::manual::ManualAdjustmentRequest, DomainError> {
        validate_business_id("adjustment_id", &self.adjustment_id)?;
        check_currency_code("currency", &self.currency)?;
        check_free_text("reason_code", &self.reason_code, MAX_REASON_CODE_LEN)?;
        let action = crate::domain::adjustment::manual::ManualAdjustmentAction::parse(&self.action)
            .ok_or_else(|| {
                DomainError::InvalidRequest(format!(
                    "unknown manual-adjustment action {:?}",
                    self.action
                ))
            })?;
        let legs = self
            .legs
            .into_iter()
            .map(|leg| {
                let account_class = leg.account_class.parse::<AccountClass>().map_err(|_| {
                    DomainError::InvalidRequest(format!(
                        "unknown account_class {:?}",
                        leg.account_class
                    ))
                })?;
                let side = leg.side.parse::<Side>().map_err(|_| {
                    DomainError::InvalidRequest(format!("unknown side {:?}", leg.side))
                })?;
                Ok(crate::domain::adjustment::manual::ManualLeg {
                    account_class,
                    side,
                    amount_minor: leg.amount_minor,
                    revenue_stream: leg.revenue_stream,
                })
            })
            .collect::<Result<Vec<_>, DomainError>>()?;
        Ok(crate::domain::adjustment::manual::ManualAdjustmentRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            adjustment_id: self.adjustment_id,
            action,
            currency: self.currency,
            legs,
            reason_code: self.reason_code,
            preparer_actor_id,
            // The approver is assigned by the dual-control approval flow, never the
            // POST body (the preparer is the authenticated subject; SoD is enforced
            // by the ApprovalService when the gross crosses the D2 threshold).
            approver_actor_id: None,
            tax: self.tax.into_iter().map(TaxBreakdown::from).collect(),
        })
    }
}

/// The `POST /manual-adjustments` response: a reference to the governed
/// adjustment's posting. A fresh post renders `201`, an idempotent replay `200`
/// (the handler reads `replayed`). Mirrors [`CreditNoteResponse`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ManualAdjustmentResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for ManualAdjustmentResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// The `POST /debit-notes` request body: an *additional charge* against a posted
/// invoice — a DIRECT split mirroring the Slice-1 invoice-post (design §4.3). The
/// target seller ledger is the body's own `tenant_id` (tenant in body, not path);
/// the `(entry, post)` PEP gate authorizes it. Idempotent on `debit_note_id` (the
/// `(tenant, DEBIT_NOTE, debit_note_id)` engine claim): a replay returns the prior
/// posting with `replayed = true`. A debit note **raises** the invoice's headroom
/// (`debit_note_total_minor += amount`); it cannot trip the headroom cap.
///
/// `amount_minor` is **incl-tax** (the single DR `AR`); `tax_minor` is the posted
/// tax evidence (CR `TAX_PAYABLE`, never recomputed); `deferred_minor` is how much
/// of the ex-tax revenue portion (`amount_minor − tax_minor`) defers to
/// `CONTRACT_LIABILITY` (the rest recognizes now to `REVENUE`). When
/// `deferred_minor > 0` the `recognition` spec drives the schedule build (D4 — the
/// SAME `ScheduleBuilder` path the invoice-post uses) and is REQUIRED (a deferred
/// note with no spec is a `400`). A malformed shape is rejected `400` by the
/// domain `validate_shape`; a closed payer is `PAYER_CLOSED`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[allow(clippy::struct_field_names)]
pub struct DebitNoteRequest {
    /// The seller tenant whose ledger this posts into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The tenant the original invoice billed (the AR owner the charge lands on).
    pub payer_tenant_id: Uuid,
    /// The `(tenant, DEBIT_NOTE, debit_note_id)` idempotency business id + the
    /// `debit_note` row PK.
    pub debit_note_id: String,
    /// The originating posted invoice (whose headroom this raises; its rows are
    /// never mutated).
    pub origin_invoice_id: String,
    /// The targeted posted invoice-item ref — anchors the freshly-built schedule's
    /// NOT-NULL `source_invoice_item_ref` when the note defers (§4.7). Required
    /// (non-empty) for a deferred note; optional (lineage only) otherwise.
    pub origin_invoice_item_ref: Option<String>,
    /// The revenue stream the charge books against (per-stream legs carry it).
    pub revenue_stream: String,
    /// ISO-4217 currency of the note (all legs share it).
    pub currency: String,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// The note amount **incl-tax**, in minor units (`>= 0`, `> 0` enforced) — the
    /// single DR `AR`.
    pub amount_minor: i64,
    /// The tax slice of `amount_minor` posted onto `TAX_PAYABLE` (`>= 0`,
    /// `<= amount_minor`). Posted tax evidence — never recomputed.
    pub tax_minor: i64,
    /// The authoritative tax breakdown — one component per `(jurisdiction,
    /// filing-period, rate)`, each posting onto its OWN `TAX_PAYABLE` leg so
    /// `tax_subbalance` disaggregates (§4.5). Empty ⇒ a single dimensionless tax leg
    /// from `tax_minor` (legacy). A non-empty breakdown MUST sum to `tax_minor` (a
    /// `400` otherwise); `tax_minor` stays the split scalar.
    pub tax: Vec<TaxBreakdownDto>,
    /// How much of the ex-tax revenue amount is deferred to `CONTRACT_LIABILITY`
    /// (`0 <= … <= amount_minor − tax_minor`); the rest recognizes now. `0` ⇒ fully
    /// recognized (no `CONTRACT_LIABILITY` line, no schedule build).
    pub deferred_minor: i64,
    /// The mandatory business reason / context code (AC #14). Non-empty.
    pub reason_code: String,
    /// The ASC 606 recognition spec (Slice 4 — the SAME shape the invoice-post
    /// item carries). REQUIRED when `deferred_minor > 0` (drives the schedule
    /// build, D4); `None` for a fully-recognized note.
    pub recognition: Option<RecognitionInputDto>,
}

impl DebitNoteRequest {
    /// Lower the wire DTO into the domain
    /// [`crate::domain::adjustment::debit_note::DebitNoteRequest`], validating the
    /// client-supplied id lengths + lowering the optional `recognition` block at
    /// the boundary. The advisory `scale` is dropped (the handler resolves the
    /// authoritative scale).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `debit_note_id` / `origin_invoice_id`
    /// (or, when present, `origin_invoice_item_ref`) is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`], or when the `recognition` block carries an invalid
    /// timing.
    pub fn into_domain(
        self,
    ) -> Result<crate::domain::adjustment::debit_note::DebitNoteRequest, DomainError> {
        validate_business_id("debit_note_id", &self.debit_note_id)?;
        validate_business_id("origin_invoice_id", &self.origin_invoice_id)?;
        if let Some(item) = &self.origin_invoice_item_ref {
            validate_business_id("origin_invoice_item_ref", item)?;
        }
        check_currency_code("currency", &self.currency)?;
        check_free_text("reason_code", &self.reason_code, MAX_REASON_CODE_LEN)?;
        let recognition = self
            .recognition
            .map(RecognitionInputDto::into_domain)
            .transpose()?;
        Ok(crate::domain::adjustment::debit_note::DebitNoteRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            debit_note_id: self.debit_note_id,
            origin_invoice_id: self.origin_invoice_id,
            origin_invoice_item_ref: self.origin_invoice_item_ref,
            revenue_stream: self.revenue_stream,
            currency: self.currency,
            amount_minor: self.amount_minor,
            tax_minor: self.tax_minor,
            tax: self.tax.into_iter().map(TaxBreakdown::from).collect(),
            deferred_minor: self.deferred_minor,
            reason_code: self.reason_code,
            recognition,
        })
    }
}

/// The `POST /debit-notes` response: a reference to the direct-split posting. A
/// fresh post renders `201`, an idempotent replay `200`. Mirrors [`PostingRefDto`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DebitNoteResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for DebitNoteResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// The `GET /invoices/{invoice_id}/exposure` response: an invoice's credit-note
/// **headroom** (the `invoice_exposure` counter) plus its **true remaining AR**
/// (the payment-reduced open receivable, design §4.7). The headroom is the room
/// left for further credit notes: `remaining_headroom_minor = original_total_minor
/// + `debit_note_total_minor` − `credit_note_total_minor`` (the slack in the
/// ``credit_note_total_minor` <= `original_total_minor` + `debit_note_total_minor``
/// CHECK, AC #24). ``open_ar_minor`` is the SEPARATE current open AR (what a credit
/// note's `CR AR` leg is capped at before the wallet remainder, K-2) — distinct
/// from the headroom (which never decreases with payments). Tenant-scoped
/// (SQL-level BOLA): an invoice with no exposure row yet (no note ever posted) —
/// or one outside the caller's subtree — yields a `404` (no existence leak).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct InvoiceExposureResponse {
    /// The invoice the exposure is for.
    pub invoice_id: String,
    /// ISO-4217 currency of the exposure counters.
    pub currency: String,
    /// The seeded original posted AR incl. tax (the headroom basis).
    pub original_total_minor: i64,
    /// The running Σ debit-note incl-tax totals (raises the headroom).
    pub debit_note_total_minor: i64,
    /// The running Σ credit-note incl-tax totals (consumes the headroom).
    pub credit_note_total_minor: i64,
    /// The remaining credit-note headroom = `original + debit − credit` (`>= 0`).
    pub remaining_headroom_minor: i64,
    /// The invoice's current open AR incl. tax (payment-reduced) — the `CR AR` cap
    /// a credit note fills before spilling to the wallet remainder. SEPARATE from
    /// the headroom.
    pub open_ar_minor: i64,
}

// ── Refund request/response DTOs (§4.4 / §5 / §7, Group G — money-OUT) ────────

/// The `POST /refunds` request body: one PSP refund phase to record (the
/// **money-out** side that unwinds a settled receipt). The target seller ledger
/// is the body's own `tenant_id` (tenant in body, not path — the vhp-core REST
/// convention); the `(entry, post)` PEP gate authorizes it (a refund posts a
/// balanced journal entry into the seller's ledger, like the credit/debit notes,
/// design §8 / Phase-1 precedent). `pattern` selects the economic shape
/// (`"A_UNALLOCATED"` / `"B_RESTORE_AR"`); `phase` the lifecycle stage
/// (`"initiated"` / `"confirmed"` / `"rejected"` / `"voided"` /
/// `"unknown_final"`). `invoice_id` is required for Pattern B (the AR it
/// restores), absent for Pattern A. `two_stage` defaults to `true` (the
/// conservative `REFUND_CLEARING` shape). `relates_to_refund_id` + `direction`
/// drive a refund-of-refund (Group E); a first-order refund omits both (direction
/// defaults to outbound). `scale` is advisory; the ledger resolves the
/// authoritative currency scale.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
// `*_id` fields mirror the domain `RefundRequest` / `refund` columns verbatim;
// renaming to satisfy `struct_field_names` would diverge from the contract.
#[allow(clippy::struct_field_names)]
pub struct RefundRequest {
    /// The seller tenant whose ledger this posts into; the PEP gate target.
    /// Carried in the body, not the path.
    pub tenant_id: Uuid,
    /// The tenant the refund returns cash to (the original payer).
    pub payer_tenant_id: Uuid,
    /// The business id of this refund — the `refund` row's surrogate PK +
    /// the `GET /refunds/{refundId}` handle (NOT the idempotency key).
    pub refund_id: String,
    /// The PSP's refund id — the idempotency grain together with `phase`.
    pub psp_refund_id: String,
    /// The lifecycle phase: `initiated` | `confirmed` | `rejected` | `voided` |
    /// `unknown_final`.
    pub phase: String,
    /// The economic pattern: `A_UNALLOCATED` | `B_RESTORE_AR`.
    pub pattern: String,
    /// The origin settled payment the refund unwinds (NOT NULL both patterns, D7).
    pub payment_id: String,
    /// The invoice whose AR the refund restores — REQUIRED for Pattern B, MUST be
    /// absent for Pattern A (validated by the domain `validate_shape`).
    pub invoice_id: Option<String>,
    /// ISO-4217 currency (all legs share it; MUST match the origin settlement's).
    pub currency: String,
    /// The cash to return, in minor units (`> 0`).
    pub amount_minor: i64,
    /// Advisory currency scale; the ledger resolves the authoritative one.
    pub scale: u8,
    /// `true` (default) ⇒ the two-stage `REFUND_CLEARING` shape; `false` ⇒ the
    /// single-step shape (D1). `None` ⇒ `true`.
    pub two_stage: Option<bool>,
    /// The prior refund this one references (refund-of-refund link); `None` for a
    /// first-order refund.
    pub relates_to_refund_id: Option<String>,
    /// The economic direction (refund-of-refund only): `OUTBOUND` | `CLAWBACK`.
    /// `None` ⇒ `OUTBOUND` for a first-order refund (the domain default is
    /// claw-back ONLY when a `relates_to_refund_id` is set; a first-order refund is
    /// always outbound, so the DTO defaults to outbound here and lets
    /// `validate_shape` require the link for an explicit claw-back).
    pub direction: Option<String>,
}

impl RefundRequest {
    /// Lower the wire DTO into the domain
    /// [`RefundRequest`](crate::domain::adjustment::refund::RefundRequest),
    /// parsing the string-typed `phase` / `pattern` / `direction` enums and
    /// validating the client-supplied id lengths at the boundary. The deeper
    /// shape rules (Pattern-B `invoice_id`, single-step phase, claw-back link) are
    /// the domain's `validate_shape`, run by the handler.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] when `phase` / `pattern` / `direction` is
    /// not a known literal, or when `refund_id` / `psp_refund_id` / `payment_id` /
    /// (when present) `invoice_id` / `relates_to_refund_id` is empty or exceeds
    /// [`MAX_BUSINESS_ID_LEN`].
    pub fn into_domain(
        self,
    ) -> Result<crate::domain::adjustment::refund::RefundRequest, DomainError> {
        use crate::domain::adjustment::refund::{RefundDirection, RefundPattern, RefundPhase};
        validate_business_id("refund_id", &self.refund_id)?;
        validate_business_id("psp_refund_id", &self.psp_refund_id)?;
        validate_business_id("payment_id", &self.payment_id)?;
        if let Some(invoice_id) = &self.invoice_id {
            validate_business_id("invoice_id", invoice_id)?;
        }
        if let Some(rel) = &self.relates_to_refund_id {
            validate_business_id("relates_to_refund_id", rel)?;
        }
        check_currency_code("currency", &self.currency)?;
        let phase = RefundPhase::parse(&self.phase).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "unknown refund phase {:?} (expected initiated|confirmed|rejected|voided|\
                 unknown_final)",
                self.phase
            ))
        })?;
        let pattern = RefundPattern::parse(&self.pattern).ok_or_else(|| {
            DomainError::InvalidRequest(format!(
                "unknown refund pattern {:?} (expected A_UNALLOCATED|B_RESTORE_AR)",
                self.pattern
            ))
        })?;
        // A first-order refund (no link) is always outbound; an explicit
        // `direction` is parsed, defaulting to OUTBOUND when omitted. The domain
        // `validate_shape` is the authority on requiring the link for a claw-back.
        let direction = match self.direction.as_deref() {
            None => RefundDirection::Outbound,
            Some(d) => RefundDirection::parse(d).ok_or_else(|| {
                DomainError::InvalidRequest(format!(
                    "unknown refund direction {d:?} (expected OUTBOUND|CLAWBACK)"
                ))
            })?,
        };
        Ok(crate::domain::adjustment::refund::RefundRequest {
            tenant_id: self.tenant_id,
            payer_tenant_id: self.payer_tenant_id,
            refund_id: self.refund_id,
            psp_refund_id: self.psp_refund_id,
            phase,
            pattern,
            payment_id: self.payment_id,
            invoice_id: self.invoice_id,
            currency: self.currency,
            amount_minor: self.amount_minor,
            two_stage: self.two_stage.unwrap_or(true),
            relates_to_refund_id: self.relates_to_refund_id,
            direction,
        })
    }
}

/// The `POST /refunds` (and the refund leg of `POST /refund-with-credit-note`)
/// response when the refund POSTED inline: a reference to the refund-stage
/// posting. A fresh stage renders `201`, an idempotent replay `200` (the handler
/// reads `replayed`). Mirrors [`SettlePaymentResponse`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RefundResponse {
    pub entry_id: Uuid,
    pub created_seq: i64,
    pub replayed: bool,
}

impl From<bss_ledger_sdk::PostingRef> for RefundResponse {
    fn from(r: bss_ledger_sdk::PostingRef) -> Self {
        Self {
            entry_id: r.entry_id,
            created_seq: r.created_seq,
            replayed: r.replayed,
        }
    }
}

/// The status token a quarantined (refund-before-payment) refund renders in
/// [`RefundQuarantinedResponse::status`] — a kebab-case literal in a normal JSON
/// body, NOT a `problem+json` error: a refund whose origin payment has not landed
/// is accepted-but-quarantined (HTTP 202), never rejected and NEVER posted
/// (design §4.4 / PRD L668 / Rev2 E-11). DISTINCT from queue-and-apply: a
/// quarantined refund only ever posts after an explicit, re-validating
/// de-quarantine.
pub const REFUND_QUARANTINED_STATUS: &str = "refund-quarantined";

/// The `POST /refunds` response when the refund references a payment with no
/// resolvable origin settlement: the refund was durably QUARANTINED (HTTP 202
/// Accepted), NOT posted. `status` is the fixed `refund-quarantined` token; `flow`
/// + `business_id` are the quarantine queue key and `quarantined_at` the intake
/// instant. No posting handle — nothing has posted (and de-quarantine re-validates
/// all §4.7 caps + the then-current D2 threshold + dispute state before any post).
/// Mirrors [`AllocationQueuedResponse`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RefundQuarantinedResponse {
    /// Always `refund-quarantined` (a normal-body kebab-case token, not an error).
    pub status: String,
    /// The quarantine queue flow (the `REFUND_QUARANTINE` literal).
    pub flow: String,
    /// The quarantine/dedup business id — `psp_refund_id:phase`.
    pub business_id: String,
    /// When the intake durably quarantined the request.
    #[serde(with = "rfc3339")]
    pub quarantined_at: OffsetDateTime,
}

/// The status token a dispute-held refund renders in
/// [`RefundDisputeHeldResponse::status`] — a kebab-case literal in a normal JSON
/// body, NOT a `problem+json` error: a refund whose origin payment has an OPEN
/// dispute is accepted-but-held (HTTP 202), never rejected and NEVER posted (Z5-2 /
/// design §5). The cash leg is held until the dispute resolves WON (the hold drain
/// re-drives it) or LOST (the hold is cancelled — the chargeback already returned
/// the money).
pub const REFUND_DISPUTE_HELD_STATUS: &str = "refund-dispute-held";

/// The `POST /refunds` response when the refund's origin payment has an OPEN
/// dispute: the refund's cash leg was durably HELD (HTTP 202 Accepted), NOT posted
/// (Z5-2 / design §5). `status` is the fixed `refund-dispute-held` token; `flow` +
/// `business_id` are the dispute-hold queue key and `held_at` the intake instant. No
/// posting handle — nothing has posted (and the hold drain re-validates the dispute
/// state + all §4.7 caps + the then-current D2 threshold before any post). Mirrors
/// [`RefundQuarantinedResponse`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RefundDisputeHeldResponse {
    /// Always `refund-dispute-held` (a normal-body kebab-case token, not an error).
    pub status: String,
    /// The dispute-hold queue flow (the `REFUND_DISPUTE_HOLD` literal).
    pub flow: String,
    /// The dispute-hold/dedup business id — `psp_refund_id:phase`.
    pub business_id: String,
    /// When the intake durably held the request.
    #[serde(with = "rfc3339")]
    pub held_at: OffsetDateTime,
}

/// The `POST /refund-with-credit-note` request body: post a refund AND its paired
/// S3 credit note ATOMICALLY in one transaction as two linked entries (K-3,
/// design §4.4). The refund carries the credit note's id (`credit_note_id`) so the
/// two are linked; AR is never overstated between them (both commit or neither).
/// The target seller ledger is the body's own `tenant_id`; the `(entry, post)` PEP
/// gate authorizes it. Carries BOTH the refund request and the credit-note request
/// inline (each its own idempotency grain) — a composite, not a discriminated
/// union.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RefundWithCreditNoteRequest {
    /// The S5 refund to post (money-out).
    pub refund: RefundRequest,
    /// The paired S3 credit note to post atomically with the refund.
    pub credit_note: CreditNoteRequest,
}

/// The `POST /refund-with-credit-note` response: references to BOTH posted entries
/// (the refund + the paired credit note), committed atomically. Always `201` on a
/// fresh composite (an idempotent replay of an already-posted composite renders
/// `200`; both halves replay together since they share the post txn).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RefundWithCreditNoteResponse {
    /// The posted refund entry id.
    pub refund_entry_id: Uuid,
    /// The posted credit-note entry id (the paired second entry).
    pub credit_note_entry_id: Uuid,
    /// `true` ⇒ an idempotent replay of an already-posted composite (both halves).
    pub replayed: bool,
}

/// The `GET /refunds/{refundId}` response: the recorded refund + its clearing
/// state (Group G). Drawn from the `refund` row (the surrogate `(tenant,
/// refund_id)` grain). `clearing_state` is the two-stage `REFUND_CLEARING` drain
/// state (`PENDING` ⇒ stage-1 open / `SETTLED` ⇒ drained or single-step /
/// `REVERSED` ⇒ a PSP reject/void line-negated the stage-1). Tenant-scoped
/// (SQL-level BOLA): an unknown refund — or one outside the caller's subtree —
/// yields a `404` (no existence leak).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct RefundView {
    pub refund_id: String,
    pub psp_refund_id: String,
    /// The latest lifecycle phase recorded on this refund.
    pub phase: String,
    /// The economic pattern (`A_UNALLOCATED` / `B_RESTORE_AR`).
    pub pattern: String,
    pub payment_id: String,
    pub invoice_id: Option<String>,
    pub currency: String,
    pub amount_minor: i64,
    /// The `REFUND_CLEARING` drain state (`PENDING` / `SETTLED` / `REVERSED`).
    pub clearing_state: String,
    /// The refund-of-refund forward link (`None` for a first-order refund).
    pub relates_to_refund_id: Option<String>,
    /// The negated stage-1 entry id on a PSP reject/void (`None` otherwise).
    pub reverses_entry_id: Option<Uuid>,
}

impl From<crate::infra::storage::entity::refund::Model> for RefundView {
    fn from(r: crate::infra::storage::entity::refund::Model) -> Self {
        Self {
            refund_id: r.refund_id,
            psp_refund_id: r.psp_refund_id,
            phase: r.phase,
            pattern: r.pattern,
            payment_id: r.payment_id,
            invoice_id: r.invoice_id,
            currency: r.currency,
            amount_minor: r.amount_minor,
            clearing_state: r.clearing_state,
            relates_to_refund_id: r.relates_to_refund_id,
            reverses_entry_id: r.reverses_entry_id,
        }
    }
}

/// The `GET /credit-notes` / `GET /credit-notes/{creditNoteId}` response: the
/// recorded credit note (Phase 1b / read-surface §5). Drawn from the
/// `credit_note` row (the `(tenant, credit_note_id)` grain). `amount_minor` is
/// incl-tax; `recognized_part_minor` + `deferred_part_minor` are the ex-tax split
/// parts and do NOT sum to `amount_minor` (no CHECK — they mirror the entity).
/// Tenant-scoped (SQL-level BOLA): an unknown credit note — or one outside the
/// caller's subtree — yields a `404` (no existence leak). Mirrors [`RefundView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct CreditNoteView {
    pub credit_note_id: String,
    pub origin_invoice_id: String,
    /// The originating invoice item the note targets (`None` when whole-invoice).
    pub origin_invoice_item_ref: Option<String>,
    pub revenue_stream: String,
    pub currency: String,
    pub amount_minor: i64,
    /// The ex-tax recognized part of the split (does NOT sum with the deferred
    /// part to `amount_minor`).
    pub recognized_part_minor: i64,
    /// The ex-tax deferred part of the split.
    pub deferred_part_minor: i64,
    /// The schedule/split basis the `RecognizedDeferredSplitter` keyed on
    /// (`None` when the split needed no schedule basis).
    pub split_basis_ref: Option<String>,
    pub reason_code: String,
    #[serde(with = "rfc3339")]
    pub created_at_utc: OffsetDateTime,
}

impl From<crate::infra::storage::entity::credit_note::Model> for CreditNoteView {
    fn from(c: crate::infra::storage::entity::credit_note::Model) -> Self {
        Self {
            credit_note_id: c.credit_note_id,
            origin_invoice_id: c.origin_invoice_id,
            origin_invoice_item_ref: c.origin_invoice_item_ref,
            revenue_stream: c.revenue_stream,
            currency: c.currency,
            amount_minor: c.amount_minor,
            recognized_part_minor: c.recognized_part_minor,
            deferred_part_minor: c.deferred_part_minor,
            split_basis_ref: c.split_basis_ref,
            reason_code: c.reason_code,
            created_at_utc: c.created_at_utc,
        }
    }
}

/// The `GET /debit-notes` / `GET /debit-notes/{debitNoteId}` response: the
/// recorded debit note — an additional charge (Phase 1b / read-surface §5). Drawn
/// from the `debit_note` row (the `(tenant, debit_note_id)` grain). `amount_minor`
/// is incl-tax; `recognized_part_minor` + `deferred_part_minor` are the ex-tax
/// split parts and do NOT sum to `amount_minor` (no CHECK). The `debit_note`
/// table is leaner than `credit_note` (NO `revenue_stream` / `reason_code` /
/// item ref). Tenant-scoped (SQL-level BOLA): an unknown debit note — or one
/// outside the caller's subtree — yields a `404` (no existence leak). Mirrors
/// [`CreditNoteView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct DebitNoteView {
    pub debit_note_id: String,
    pub origin_invoice_id: String,
    pub currency: String,
    pub amount_minor: i64,
    /// The ex-tax recognized part of the split.
    pub recognized_part_minor: i64,
    /// The ex-tax deferred part of the split.
    pub deferred_part_minor: i64,
    #[serde(with = "rfc3339")]
    pub created_at_utc: OffsetDateTime,
}

impl From<crate::infra::storage::entity::debit_note::Model> for DebitNoteView {
    fn from(d: crate::infra::storage::entity::debit_note::Model) -> Self {
        Self {
            debit_note_id: d.debit_note_id,
            origin_invoice_id: d.origin_invoice_id,
            currency: d.currency,
            amount_minor: d.amount_minor,
            recognized_part_minor: d.recognized_part_minor,
            deferred_part_minor: d.deferred_part_minor,
            created_at_utc: d.created_at_utc,
        }
    }
}

/// The `GET /disputes` / `GET /disputes/{disputeId}` response: the chargeback
/// dispute's current state (read-surface R3). Drawn from the `ledger_dispute` row
/// (the `(tenant, dispute_id)` grain) — its chosen `variant` (`CASH_HOLD` /
/// `AR_RECLASS`), the current `cycle` + `last_phase` (`OPENED` / `WON` / `LOST`),
/// the `disputed_amount_minor`, and the `cash_hold_minor` actually moved into
/// `DISPUTE_HOLD` at open (`0` for `AR_RECLASS`). The persisted `version` (the
/// optimistic-concurrency counter) is INTERNAL and not surfaced. Tenant-scoped
/// (SQL-level BOLA): an unknown dispute — or one outside the caller's subtree —
/// yields a `404` (no existence leak). Mirrors [`RefundView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct DisputeView {
    pub dispute_id: String,
    pub payment_id: String,
    pub currency: String,
    /// The chosen variant (`CASH_HOLD` ⇒ cash moved to `DISPUTE_HOLD` at open /
    /// `AR_RECLASS` ⇒ AR reclassed `ACTIVE`→`DISPUTED`, no cash leg).
    pub variant: String,
    /// The latest phase recorded on this dispute (`OPENED` / `WON` / `LOST`).
    pub last_phase: String,
    /// The dispute cycle (re-opens advance it; the `last_phase` is at this cycle).
    pub cycle: i32,
    pub disputed_amount_minor: i64,
    /// The cash held in `DISPUTE_HOLD` at open (`0` for `AR_RECLASS`) — the size
    /// the `won`/`lost` outcome releases / forfeits.
    pub cash_hold_minor: i64,
}

impl From<crate::infra::storage::entity::dispute::Model> for DisputeView {
    fn from(d: crate::infra::storage::entity::dispute::Model) -> Self {
        Self {
            dispute_id: d.dispute_id,
            payment_id: d.payment_id,
            currency: d.currency,
            variant: d.variant,
            last_phase: d.last_phase,
            cycle: d.cycle,
            disputed_amount_minor: d.disputed_amount_minor,
            cash_hold_minor: d.cash_hold_minor,
        }
    }
}

/// The `GET /recognition-runs` / `GET /recognition-runs/{run_id}` response: one
/// recorded ASC 606 recognition run (read-surface R4). Drawn from the
/// `recognition_run` row (the `(tenant, period_id, run_id)` grain — the run is the
/// orchestration wrapper that released a period's due segments). `status` is the
/// run lifecycle (`RUNNING` ⇒ in-flight / `DONE` ⇒ completed / `FAILED` ⇒ aborted);
/// `started_at_utc` is when the run began. Tenant-scoped (SQL-level BOLA): an
/// unknown run — or one outside the caller's subtree — yields a `404` (no existence
/// leak). Mirrors [`DisputeView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RecognitionRunView {
    pub run_id: Uuid,
    /// The fiscal period (`YYYYMM`) the run released due segments for.
    pub period_id: String,
    /// The run lifecycle (`RUNNING` / `DONE` / `FAILED`).
    pub status: String,
    #[serde(with = "rfc3339")]
    pub started_at_utc: OffsetDateTime,
}

impl From<crate::infra::storage::entity::recognition_run::Model> for RecognitionRunView {
    fn from(r: crate::infra::storage::entity::recognition_run::Model) -> Self {
        Self {
            run_id: r.run_id,
            period_id: r.period_id,
            status: r.status,
            started_at_utc: r.started_at_utc,
        }
    }
}

/// The `GET /payments/{payment_id}/settlement` response: the per-payment money-out
/// serialization counters (read-surface R4). Drawn from the `payment_settlement`
/// row (the `(tenant, payment_id)` grain) — the settled / fee / allocated /
/// refunded / clawed-back running totals the money-out caps serialize against.
/// The persisted `version` (the optimistic-concurrency counter) is INTERNAL and
/// not surfaced. Tenant-scoped (SQL-level BOLA): a payment that was never settled
/// — or one outside the caller's subtree — yields a `404` (no existence leak).
/// Mirrors [`RefundView`].
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
#[toolkit_macros::api_dto(response)]
pub struct SettlementView {
    pub payment_id: String,
    pub currency: String,
    /// The gross settled amount recorded for the payment (money-in).
    pub settled_minor: i64,
    /// The PSP fee withheld from the settled receipt.
    pub fee_minor: i64,
    /// The portion of the pool already drained to open AR (money-out).
    pub allocated_minor: i64,
    /// The portion already returned to the payer via refunds.
    pub refunded_minor: i64,
    /// The portion refunded from the still-unallocated pool (Pattern A).
    pub refunded_unallocated_minor: i64,
    /// The portion clawed back (a refund-of-refund / PSP claw-back).
    pub clawed_back_minor: i64,
}

impl From<crate::infra::storage::entity::payment_settlement::Model> for SettlementView {
    fn from(s: crate::infra::storage::entity::payment_settlement::Model) -> Self {
        Self {
            payment_id: s.payment_id,
            currency: s.currency,
            settled_minor: s.settled_minor,
            fee_minor: s.fee_minor,
            allocated_minor: s.allocated_minor,
            refunded_minor: s.refunded_minor,
            refunded_unallocated_minor: s.refunded_unallocated_minor,
            clawed_back_minor: s.clawed_back_minor,
        }
    }
}

/// One row of the `GET /journal-entries` header list (read-surface R5). A
/// LIGHTWEIGHT projection of a `journal_entry` HEADER — the entry coordinate +
/// audit dims a caller filters / cross-cuts on (`source_doc_type` ⇒ all
/// `MANUAL_ADJUSTMENT` / `REFUND` / `CREDIT_NOTE` entries, `source_business_id`
/// ⇒ all entries of one business document, `period_id` ⇒ all entries of a
/// period). Carries NO lines and NONE of the tamper-evidence hash-chain fields
/// (`row_hash` / `prev_hash` / `prev_entry_id` / `prev_period_id` are chain
/// internals); a caller that needs the lines reads the full entry via
/// `GET /journal-entries/{entryId}` (which returns the richer [`EntryDto`]).
/// Drawn from the `journal_entry` row (the `(entry_id, tenant_id, period_id)`
/// grain; `entry_id` is the default keyset-order column). Tenant-scoped
/// (SQL-level BOLA): the page never contains a foreign-tenant header (no
/// existence leak). Mirrors [`RefundView`].
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct EntryHeaderView {
    pub entry_id: Uuid,
    /// The fiscal period (`YYYYMM`) the entry posted into.
    pub period_id: String,
    /// The entry's booking currency (the header-level currency).
    pub entry_currency: String,
    /// The business document class (`INVOICE_POST` / `MANUAL_ADJUSTMENT` /
    /// `REFUND` / `CREDIT_NOTE` / …) — the primary cross-cut filter dim.
    pub source_doc_type: String,
    /// The originating business document id (the invoice / note / refund ref).
    pub source_business_id: String,
    /// The reversed entry id when this header is itself a reversal (`None`
    /// otherwise).
    pub reverses_entry_id: Option<Uuid>,
    #[serde(with = "rfc3339")]
    pub posted_at_utc: OffsetDateTime,
    pub effective_at: NaiveDate,
    /// The posting origin (the channel / driver that emitted the entry).
    pub origin: String,
    /// The DB-assigned per-tenant monotonic posting sequence.
    pub created_seq: i64,
}

impl From<crate::infra::storage::entity::journal_entry::Model> for EntryHeaderView {
    fn from(e: crate::infra::storage::entity::journal_entry::Model) -> Self {
        Self {
            entry_id: e.entry_id,
            period_id: e.period_id,
            entry_currency: e.entry_currency,
            source_doc_type: e.source_doc_type,
            source_business_id: e.source_business_id,
            reverses_entry_id: e.reverses_entry_id,
            posted_at_utc: e.posted_at_utc,
            effective_at: e.effective_at,
            origin: e.origin,
            created_seq: e.created_seq,
        }
    }
}

/// The `GET /payers/{payer_tenant_id}/state` response: a payer's ledger lifecycle
/// state for the caller's seller tenant (read-surface). `lifecycle_state` is the
/// payer's current state (`OPEN` / `CLOSED`); `closed_with_open_balance` records
/// whether a close was approved over an outstanding balance (the dual-control
/// disposition). Tenant-scoped (SQL-level BOLA): a payer with no recorded state —
/// or outside the caller's subtree — yields a `404` (no existence leak).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_field_names)]
pub struct PayerStateView {
    /// The payer tenant whose lifecycle state this is.
    pub payer_tenant_id: Uuid,
    /// The payer's current ledger lifecycle state (`OPEN` / `CLOSED`).
    pub lifecycle_state: String,
    /// `true` when the payer was CLOSED while still holding an outstanding balance
    /// (an approved dual-control disposition); `false` for a clean close.
    pub closed_with_open_balance: bool,
    /// The approver who signed off a close-with-balance (`None` for a clean / open
    /// payer).
    pub approved_by: Option<Uuid>,
    /// When the lifecycle state last changed (`None` if never transitioned).
    #[serde(default, with = "rfc3339::option")]
    pub changed_at: Option<OffsetDateTime>,
}

impl From<crate::infra::storage::entity::payer_state::Model> for PayerStateView {
    fn from(m: crate::infra::storage::entity::payer_state::Model) -> Self {
        Self {
            payer_tenant_id: m.payer_tenant_id,
            lifecycle_state: m.lifecycle_state,
            closed_with_open_balance: m.closed_with_open_balance,
            approved_by: m.approved_by,
            changed_at: m.changed_at,
        }
    }
}

/// The `GET /dual-control-policy` response (read-surface): the tenant's EFFECTIVE
/// dual-control threshold policy — the version in force now (greatest
/// `effective_from <= now`, highest `version` on a tie), or the ratified platform
/// defaults when the tenant has set no policy row. `is_default` is `true` in the
/// latter case (`version` / `effective_from` are then `None`). Tenant-scoped
/// (SQL-level BOLA): a tenant outside the caller's subtree reads as the platform
/// defaults — the thresholds are public constants, so no existence/value leak.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DualControlPolicyView {
    /// The D2 amount threshold in USD-equivalent minor units: a governed money-out
    /// / grant at or above this needs preparer→approver sign-off.
    pub d2_threshold_minor: i64,
    /// The A6 material-backdating window in business days.
    pub a6_backdating_biz_days: i32,
    /// The TTL (seconds) a fresh `PENDING` / `NEEDS_REWORK` approval lives before
    /// it expires.
    pub pending_ttl_seconds: i64,
    /// The `effective_from` instant of the version in force (`None` when the
    /// platform defaults apply — the tenant has no row).
    #[serde(default, with = "rfc3339::option")]
    pub effective_from: Option<OffsetDateTime>,
    /// The `version` number in force (`None` when the platform defaults apply).
    pub version: Option<i64>,
    /// `true` when no tenant row applies and these are the ratified platform
    /// defaults; `false` when a configured version is in force.
    pub is_default: bool,
}

impl DualControlPolicyView {
    /// Build from the effective policy version (`None` ⇒ the ratified platform
    /// defaults, `is_default = true`).
    #[must_use]
    pub fn from_effective(effective: Option<PolicyVersion>) -> Self {
        // No in-force row ⇒ render the ratified platform defaults (guard clause).
        let Some(v) = effective else {
            let d = DualControlPolicy::DEFAULT;
            return Self {
                d2_threshold_minor: d.d2_threshold_minor,
                a6_backdating_biz_days: d.a6_backdating_biz_days,
                pending_ttl_seconds: d.pending_ttl_seconds,
                effective_from: None,
                version: None,
                is_default: true,
            };
        };
        Self {
            d2_threshold_minor: v.policy.d2_threshold_minor,
            a6_backdating_biz_days: v.policy.a6_backdating_biz_days,
            pending_ttl_seconds: v.policy.pending_ttl_seconds,
            effective_from: Some(v.effective_from),
            version: Some(v.version),
            is_default: false,
        }
    }
}

// ── FX & multi-currency (Slice 5) ────────────────────────────────────────────

/// Light boundary check for a currency code on an FX ingest: non-empty, ASCII,
/// ≤ 10 chars (the gear admits non-ISO/crypto codes — same envelope as the
/// `read_unallocated` query guard). An unvalidated code would silently match zero
/// rows at lock time instead of a clean 400 here.
fn check_currency_code(field: &str, code: &str) -> Result<(), DomainError> {
    if code.is_empty() || code.len() > 10 || !code.is_ascii() {
        return Err(DomainError::InvalidRequest(format!(
            "{field} must be a non-empty ASCII code of at most 10 chars, got {code:?}"
        )));
    }
    Ok(())
}

/// Max bytes of a machine `reason_code` (a short enumerated-style token).
const MAX_REASON_CODE_LEN: usize = 64;
/// Max bytes of a human free-text `reason` / `description` note.
const MAX_FREE_TEXT_LEN: usize = 4096;

/// Light boundary cap for a persisted free-text field: reject a value whose byte
/// length exceeds `max` (mirrors [`check_currency_code`]). An unbounded note would
/// otherwise blow past its storage column as a 500 instead of a clean 400 here.
fn check_free_text(field: &str, value: &str, max: usize) -> Result<(), DomainError> {
    if value.len() > max {
        return Err(DomainError::InvalidRequest(format!(
            "{field} must be at most {max} bytes, got {}",
            value.len()
        )));
    }
    Ok(())
}

/// Secondary manual / seed ingest of one FX rate into the local `ledger_fx_rate`
/// store (the primary path is the `RateProviderV1` plugin pull, design §4.6 /
/// decision 2). Upsert-keyed on `(tenant_id, base_currency, quote_currency,
/// provider)`: re-posting the same tuple overwrites the quote (`rate_micro` /
/// `as_of` / `fallback_order`) — idempotent on `(tenant, base, quote, provider,
/// as_of)`. `tenant_id` rides the body (the vhp-core write convention, no tenant
/// in the path).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct FxRateIngestRequest {
    /// The seller tenant whose local rate store receives the row.
    pub tenant_id: Uuid,
    /// Transaction-side ISO-4217 code (the `base` of the `base → quote` rate).
    pub base_currency: String,
    /// Functional-side ISO-4217 code (the `quote`).
    pub quote_currency: String,
    /// Provider id recorded verbatim (the fallback-order key, e.g. `"ecb"`).
    pub provider: String,
    /// The rate as a fixed-precision multiplier (functional per unit transaction
    /// × 1e6). Must be `> 0`.
    pub rate_micro: i64,
    /// The publication timestamp that drives the staleness rule.
    #[serde(with = "rfc3339")]
    pub as_of: OffsetDateTime,
    /// The provider's precedence rank (0 = primary); defaults to `0` when omitted.
    pub fallback_order: Option<i32>,
}

impl FxRateIngestRequest {
    /// Validate the ingest and return the resolved `fallback_order` (defaulted to
    /// `0`). Rejects empty/oversized currency or provider codes, a non-positive
    /// `rate_micro`, a negative `fallback_order`, and an identity (`base ==
    /// quote`) pair (a no-op rate the lock-time short-circuit never reads).
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] on any boundary violation (rendered 400).
    pub fn validate(&self) -> Result<i32, DomainError> {
        check_currency_code("base_currency", &self.base_currency)?;
        check_currency_code("quote_currency", &self.quote_currency)?;
        if self.base_currency == self.quote_currency {
            return Err(DomainError::InvalidRequest(format!(
                "base_currency and quote_currency must differ (identity rate {} is never \
                 locked — single-currency entries leave functional NULL)",
                self.base_currency
            )));
        }
        if self.provider.is_empty() || self.provider.len() > 64 {
            return Err(DomainError::InvalidRequest(format!(
                "provider must be a non-empty code of at most 64 chars, got {:?}",
                self.provider
            )));
        }
        if self.rate_micro <= 0 {
            return Err(DomainError::InvalidRequest(format!(
                "rate_micro must be > 0, got {}",
                self.rate_micro
            )));
        }
        let fallback_order = self.fallback_order.unwrap_or(0);
        if fallback_order < 0 {
            return Err(DomainError::InvalidRequest(format!(
                "fallback_order must be >= 0, got {fallback_order}"
            )));
        }
        Ok(fallback_order)
    }
}

/// Confirmation of a stored FX rate (echoes the now-current `ledger_fx_rate`
/// row's key + quote so a seeder can read back what it ingested).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FxRateIngestResponse {
    pub tenant_id: Uuid,
    pub base_currency: String,
    pub quote_currency: String,
    pub provider: String,
    pub rate_micro: i64,
    #[serde(with = "rfc3339")]
    pub as_of: OffsetDateTime,
    pub fallback_order: i32,
}

/// An immutable `ledger_fx_rate_snapshot` row — the frozen rate a journal line's
/// `rate_snapshot_ref` points at, reproducing its exact lock-time translation.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct FxRateSnapshotResponse {
    pub rate_id: Uuid,
    pub tenant_id: Uuid,
    pub base_currency: String,
    pub quote_currency: String,
    pub rate_micro: i64,
    #[serde(with = "rfc3339")]
    pub as_of: OffsetDateTime,
    pub provider: String,
    pub stale: bool,
    pub fallback_order: i32,
    pub triangulated_via: Option<String>,
}

impl From<crate::infra::storage::entity::fx_rate_snapshot::Model> for FxRateSnapshotResponse {
    fn from(m: crate::infra::storage::entity::fx_rate_snapshot::Model) -> Self {
        Self {
            rate_id: m.rate_id,
            tenant_id: m.tenant_id,
            base_currency: m.base_currency,
            quote_currency: m.quote_currency,
            rate_micro: m.rate_micro,
            as_of: m.as_of,
            provider: m.provider,
            stale: m.stale,
            fallback_order: m.fallback_order,
            triangulated_via: m.triangulated_via,
        }
    }
}

/// Trigger an unrealized (Mode-B) revaluation for one period across the monetary
/// scopes `{AR, UNALLOCATED, REUSABLE_CREDIT}` (design §4.5). `tenant_id` rides
/// the body (the vhp-core write convention). Each scope is idempotent on
/// `(tenant, period_id, scope)` — re-posting the same period replays the
/// already-posted scopes. A no-op when the tenant is Mode-A
/// (`revaluation_enabled = false`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RevaluationRunRequest {
    /// The seller tenant whose foreign-currency monetary positions are remeasured.
    pub tenant_id: Uuid,
    /// The period to revalue (`YYYYMM`). Must be an OPEN period at run time (the
    /// run posts INTO it); a CLOSED/absent period is rejected by the posting gate.
    pub period_id: String,
}

impl RevaluationRunRequest {
    /// Validate the request: a well-formed `YYYYMM` period id.
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] on a malformed period id (rendered 400).
    pub fn validate(&self) -> Result<(), DomainError> {
        if crate::domain::period::period_end_utc(&self.period_id).is_none() {
            return Err(DomainError::InvalidRequest(format!(
                "period_id must be a valid YYYYMM, got {:?}",
                self.period_id
            )));
        }
        Ok(())
    }
}

/// The outcome of a revaluation run — one entry per monetary scope.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RevaluationRunResponse {
    /// The period that was revalued (`YYYYMM`).
    pub period_id: String,
    /// Per-scope outcomes (`AR` / `UNALLOCATED` / `REUSABLE_CREDIT`).
    pub scopes: Vec<RevaluationScopeOutcomeDto>,
}

/// One monetary scope's revaluation outcome (aggregated across its per-payer
/// entries — an entry spans only one payer, so a scope fans out over payers).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RevaluationScopeOutcomeDto {
    /// The monetary scope (`AR` / `UNALLOCATED` / `REUSABLE_CREDIT`).
    pub scope: String,
    /// The outcome: `posted` (entries written), `nothing_to_post` (no
    /// cross-currency movement / a full idempotent replay), or `disabled` (Mode A).
    pub status: String,
    /// Entries freshly written this call (a full idempotent replay reports `0`).
    pub entries: i64,
    /// Grains moved across those entries (a forward run only; `0` for a no-op).
    pub grains: i64,
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
