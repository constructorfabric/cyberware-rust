//! `ProvisioningService` — the transactional seller-provisioning seed. One
//! sea-orm transaction runs the additive SELECT-then-INSERT of the chart of
//! accounts, non-ISO currency scales, the fiscal-calendar config, and the
//! initial OPEN fiscal period. The fiscal-calendar fields are validated and
//! the initial `period_id` derived BEFORE the transaction opens (pure
//! `domain::provisioning::plan`), so a malformed calendar never takes a write
//! lock.
//!
//! ## Error handling across the transaction boundary
//!
//! [`DBProvider::transaction`] fixes the closure error type to [`DbError`].
//! The only distinct business rejection that must survive the boundary is a
//! scale that exceeds `i64` headroom ([`RepoError::ScaleOutOfRange`]): the
//! closure encodes it into a sentinel [`DbError::Sea`] (`DbErr::Custom`) and
//! returns `Err`, forcing a rollback; once `transaction()` returns the
//! sentinel is decoded back into [`DomainError::ScaleOutOfRange`]. Every
//! other [`RepoError`]/[`DbError`] is an infrastructure fault and surfaces as
//! [`DomainError::Internal`].

use bss_ledger_sdk::{AccountInfo, ProvisionOutcome, ProvisionRequest};
use sea_orm::DbErr;
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{
    AccountRow, CurrencyScaleRow, FiscalCalendarRow, FiscalPeriodRow, RepoError,
};
use crate::domain::provisioning::plan;
use crate::domain::status::{LIFECYCLE_OPEN, PERIOD_STATUS_OPEN};
use crate::infra::storage::repo::ReferenceRepo;
use time::OffsetDateTime;

/// Record-separator framing a sentinel-encoded scale-out-of-range business
/// error inside a `DbErr::Custom` payload: `BSS_PROV_SCALE_OOR␟<currency>`.
const SENTINEL_TAG: &str = "BSS_PROV_SCALE_OOR";
/// Unit-separator used inside the sentinel payload.
const SENTINEL_SEP: char = '\u{1f}';

/// The transactional seller-provisioning service.
#[derive(Clone)]
pub struct ProvisioningService {
    db: DBProvider<DbError>,
    reference: ReferenceRepo,
}

impl ProvisioningService {
    /// Build a `ProvisioningService` and its reference repository from one
    /// provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        let reference = ReferenceRepo::new(db.clone());
        Self { db, reference }
    }

    /// Provision a seller legal-entity in one transaction: additively seed the
    /// chart of accounts, non-ISO currency scales, the fiscal-calendar config,
    /// and the initial OPEN fiscal period. Idempotent — existing rows are a
    /// no-op and reported as "existing".
    ///
    /// # Errors
    /// [`DomainError::InvalidRequest`] on a malformed fiscal calendar;
    /// [`DomainError::ScaleOutOfRange`] when a non-ISO scale exceeds the
    /// supported headroom; [`DomainError::Internal`] on a storage/
    /// transaction failure.
    pub async fn provision(&self, req: ProvisionRequest) -> Result<ProvisionOutcome, DomainError> {
        // --- PRE-TRANSACTION (fail fast, no writes) ---
        plan::validate_calendar(&req.fiscal_calendar)?;
        let period_id = plan::initial_period_id(OffsetDateTime::now_utc());

        // --- TRANSACTION ---
        // Clone the repo + request into the closure so the `transaction`
        // borrow of `self.db` does not conflict with the captured repo.
        let reference = self.reference.clone();
        let period_id_for_txn = period_id.clone();
        let result = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    Self::provision_in_txn(&reference, txn, req, period_id_for_txn).await
                })
            })
            .await;

        match result {
            Ok(outcome) => Ok(outcome),
            Err(db_err) => Err(decode_provision_error(&db_err)),
        }
    }

    /// The in-transaction seed body. The scale-out-of-range business rejection
    /// is encoded as a sentinel `DbError` so the closure error type stays
    /// `DbError` while still forcing a rollback.
    async fn provision_in_txn(
        reference: &ReferenceRepo,
        txn: &toolkit_db::secure::DbTx<'_>,
        req: ProvisionRequest,
        period_id: String,
    ) -> Result<ProvisionOutcome, DbError> {
        let tenant_id = req.tenant_id;
        // v1: one legal entity per tenant — the LE is the tenant.
        let legal_entity_id = req.tenant_id;
        let timezone = req.fiscal_calendar.timezone.clone();

        // Chart of accounts.
        let mut accounts_created: u32 = 0;
        let mut accounts_existing: u32 = 0;
        // The accounts THIS call creates (re-existing ones are not returned —
        // the full chart is discoverable via `list_accounts`).
        let mut accounts: Vec<AccountInfo> = Vec::new();
        for account in req.accounts {
            // Capture the coordinate before `account` moves into the row.
            let account_class = account.account_class;
            let currency = account.currency.clone();
            let revenue_stream = account.revenue_stream.clone();
            let row = AccountRow {
                account_id: Uuid::now_v7(),
                tenant_id,
                legal_entity_id,
                account_class: account.account_class.as_str().to_owned(),
                currency: account.currency,
                revenue_stream: account.revenue_stream,
                normal_side: account.normal_side.as_str().to_owned(),
                may_go_negative: account.may_go_negative,
                lifecycle_state: LIFECYCLE_OPEN.to_owned(),
            };
            let (account_id, created) = reference
                .insert_account_if_absent_txn(txn, row)
                .await
                .map_err(repo_to_db)?;
            if created {
                accounts_created += 1;
                accounts.push(AccountInfo {
                    account_id,
                    account_class,
                    currency,
                    revenue_stream,
                    lifecycle_state: LIFECYCLE_OPEN.to_owned(),
                });
            } else {
                accounts_existing += 1;
            }
        }

        // Non-ISO currency scales.
        let mut scales_created: u32 = 0;
        let mut scales_existing: u32 = 0;
        for scale in req.currency_scales {
            let row = CurrencyScaleRow {
                tenant_id,
                currency: scale.currency,
                minor_units: i16::from(scale.minor_units),
                plausible_max_major: scale
                    .plausible_max_major
                    .unwrap_or(crate::domain::money::DEFAULT_PLAUSIBLE_MAX_MAJOR),
                source: scale.source,
            };
            if reference
                .insert_currency_scale_if_absent_txn(txn, row)
                .await
                .map_err(repo_to_db)?
            {
                scales_created += 1;
            } else {
                scales_existing += 1;
            }
        }

        // Fiscal-calendar config.
        let calendar_created = reference
            .upsert_fiscal_calendar_if_absent_txn(
                txn,
                FiscalCalendarRow {
                    tenant_id,
                    legal_entity_id,
                    fiscal_tz: timezone.clone(),
                    granularity: req.fiscal_calendar.granularity.as_str().to_owned(),
                    fy_start_month: i16::from(req.fiscal_calendar.fy_start_month),
                    functional_currency: req.fiscal_calendar.functional_currency.clone(),
                },
            )
            .await
            .map_err(repo_to_db)?;

        // Initial OPEN fiscal period.
        let period_created = reference
            .insert_fiscal_period_if_absent_txn(
                txn,
                FiscalPeriodRow {
                    tenant_id,
                    legal_entity_id,
                    period_id: period_id.clone(),
                    fiscal_tz: timezone,
                    status: PERIOD_STATUS_OPEN.to_owned(),
                },
            )
            .await
            .map_err(repo_to_db)?;

        Ok(ProvisionOutcome {
            accounts,
            accounts_created,
            accounts_existing,
            scales_created,
            scales_existing,
            calendar_created,
            period_id,
            period_created,
        })
    }
}

/// Map a [`RepoError`] into a `DbError` for the transaction closure: a
/// scale-out-of-range rejection is sentinel-encoded (decoded back to
/// [`DomainError::ScaleOutOfRange`]); everything else is an infrastructure
/// fault carried as a plain `DbErr::Custom`.
fn repo_to_db(e: RepoError) -> DbError {
    match e {
        RepoError::ScaleOutOfRange(currency) => scale_out_of_range(&currency),
        other => infra(other.to_string()),
    }
}

/// Encode a scale-out-of-range business rejection as a sentinel `DbError` so
/// the transaction closure rolls back yet preserves the offending currency for
/// decoding after `transaction()` returns.
fn scale_out_of_range(currency: &str) -> DbError {
    DbError::Sea(DbErr::Custom(format!(
        "{SENTINEL_TAG}{SENTINEL_SEP}{currency}"
    )))
}

/// Encode an internal (infrastructure) failure as a non-sentinel `DbError`.
fn infra(message: impl Into<String>) -> DbError {
    DbError::Sea(DbErr::Custom(message.into()))
}

/// Decode a `DbError` returned from `transaction()` back into a
/// [`DomainError`]: a sentinel-tagged `DbErr::Custom` yields
/// [`DomainError::ScaleOutOfRange`]; any other `DbError` is an
/// infrastructure fault.
fn decode_provision_error(db_err: &DbError) -> DomainError {
    if let DbError::Sea(DbErr::Custom(payload)) = db_err
        && let Some(currency) = payload.strip_prefix(&format!("{SENTINEL_TAG}{SENTINEL_SEP}"))
    {
        return DomainError::ScaleOutOfRange(format!(
            "currency {currency} scale exceeds the supported headroom"
        ));
    }
    DomainError::Internal(db_err.to_string())
}
