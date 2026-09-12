//! `PeriodOpenJob` — fiscal-period-open automation.
//!
//! Ensures the current and next `period_id` (YYYYMM, +1 month — no
//! `chrono-tz`) exist with `status = OPEN` for every legal entity that has a
//! `fiscal_calendar` row. Idempotent insert. System-context / cross-tenant:
//! the calendar LIST is unscoped (`list_all_fiscal_calendars`), while each
//! period INSERT is scoped per-calendar by `insert_fiscal_period_if_absent_txn`
//! (`AccessScope::for_tenant(row.tenant_id)`).

use sea_orm::DbErr;
use toolkit_db::{DBProvider, DbError};
use tracing::warn;

use crate::domain::model::FiscalPeriodRow;
use crate::domain::status::PERIOD_STATUS_OPEN;
use crate::infra::storage::repo::ReferenceRepo;
use time::OffsetDateTime;

/// Granularity literal honored by the MVP period-open job (monthly only).
const GRANULARITY_MONTH: &str = "MONTH";

/// Outcome of one period-open pass.
pub struct PeriodOpenReport {
    /// Number of `fiscal_period` rows newly created across all calendars.
    pub periods_created: u64,
}

/// Ensures current + next fiscal periods exist for every legal entity with a
/// calendar.
pub struct PeriodOpenJob {
    db: DBProvider<DbError>,
}

impl PeriodOpenJob {
    /// Build the job over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Ensure the current and next fiscal periods exist for every legal
    /// entity with a calendar (idempotent). Only `MONTH` granularity is
    /// honored in the MVP; any other granularity is logged and skipped.
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure (DB unreachable, list or
    /// insert failure).
    pub async fn run(&self) -> anyhow::Result<PeriodOpenReport> {
        let repo = ReferenceRepo::new(self.db.clone());
        let calendars = repo.list_all_fiscal_calendars().await?;

        let mut periods_created: u64 = 0;
        for cal in calendars {
            if cal.granularity != GRANULARITY_MONTH {
                warn!(
                    tenant_id = %cal.tenant_id,
                    legal_entity_id = %cal.legal_entity_id,
                    granularity = %cal.granularity,
                    "bss-ledger: period-open skipping non-MONTH calendar (MVP supports MONTH only)"
                );
                continue;
            }

            let cur = crate::domain::period::period_id_for(OffsetDateTime::now_utc());
            let next = crate::domain::period::next_period_id(&cur);
            // Flatten the current + (optional) next period ids.
            let period_ids: Vec<String> = [Some(cur), next].into_iter().flatten().collect();

            // One transaction per calendar: insert each absent period and carry
            // the created-count out of the closure on the COMMIT (`Ok`) path.
            // The closure error type is fixed to `DbError`, so a `RepoError` is
            // encoded as a `DbErr::Custom` and surfaced after the transaction.
            let repo = repo.clone();
            let tenant_id = cal.tenant_id;
            let legal_entity_id = cal.legal_entity_id;
            let fiscal_tz = cal.fiscal_tz.clone();
            let created: u64 = self
                .db
                .transaction(move |txn| {
                    Box::pin(async move {
                        let mut created = 0_u64;
                        for period_id in period_ids {
                            let inserted = repo
                                .insert_fiscal_period_if_absent_txn(
                                    txn,
                                    FiscalPeriodRow {
                                        tenant_id,
                                        legal_entity_id,
                                        period_id,
                                        fiscal_tz: fiscal_tz.clone(),
                                        status: PERIOD_STATUS_OPEN.to_owned(),
                                    },
                                )
                                .await
                                .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                            if inserted {
                                created += 1;
                            }
                        }
                        Ok::<u64, DbError>(created)
                    })
                })
                .await
                .map_err(|e| anyhow::anyhow!("period-open insert: {e}"))?;
            periods_created += created;
        }

        Ok(PeriodOpenReport { periods_created })
    }
}
