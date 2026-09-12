//! `AgedAlarmJob` — periodic `Warn`-severity alarms for work that has aged past
//! a threshold without clearing (architecture §6). Three families:
//!
//! - **`AGED_ALLOCATION_QUEUE`** — a `PAYMENT_ALLOCATE` queue row still `QUEUED`
//!   whose `queued_at` is older than [`AGED_THRESHOLD_SECS`] (its settlement never
//!   landed, or the drain keeps failing).
//! - **`DISPUTE_PHASE_QUEUED`** — the same for a `CHARGEBACK` queue row (an
//!   out-of-order `won`/`lost` awaiting its `opened`).
//! - **`AGED_UNALLOCATED`** — an `unallocated_balance` grain still holding cash
//!   (`balance_minor > 0`) whose OLDEST contributing `UNALLOCATED` journal line
//!   posted longer ago than the threshold (unapplied receipts nobody allocated).
//!
//! Unlike the hard `TieOutJob` invariants (which are `Critical`), aged alarms are
//! `Warn`: they flag *latency*, not a books defect — the queue/cache is correct,
//! just stale. They are re-emitted every tick until the aged item clears, exactly
//! like a re-detected tie-out variance.
//!
//! ## Age proxy for `AGED_UNALLOCATED` (resolves G-P5a — DECIDED)
//! `unallocated_balance` carries no age timestamp; `last_entry_seq` points at the
//! LATEST contributing entry (wrong direction). The age proxy is therefore the
//! **oldest** contributing `UNALLOCATED` line's post time: the gear has no
//! `posted_at_utc` on `journal_line` (it lives on `journal_entry`), so the job
//! reads both, builds an `entry_id -> posted_at_utc` map, groups the tenant's
//! `UNALLOCATED` lines by the unallocated grain `(payer, account, currency)`
//! (mirroring `BalanceProjector::derive_grains`), and takes the MIN post time per
//! grain. A grain is flagged iff that min age exceeds the threshold AND the cache
//! `balance_minor > 0` (cash still parked). No migration.
//!
//! ## System-context / cross-tenant (mirrors `TieOutJob` + `QueueApplierJob`)
//! The aged-queue scan reads the UNSCOPED cross-tenant candidate feed
//! ([`PendingQueueRepo::list_all_due`]); the unallocated scan enumerates tenants
//! from `unallocated_balance` under [`AccessScope::allow_all`] and re-reads each
//! tenant's lines + cache under [`AccessScope::for_tenant`]. All aggregation is in
//! memory (the gear has no DB-side aggregate access — see the `TieOutJob` docs).
//! A per-tenant read failure is isolated (logged, the pass continues) so one
//! flaky tenant doesn't abort the tick. Age is measured against
//! [`chrono::Utc::now`] — this is a live periodic tick, not a deterministic
//! replay, so wall-clock `now` is the correct reference here.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait};
use time::Duration;
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::infra::events::payloads::{
    AffectedItem, AlarmCategory, AlarmSeverity, LedgerInvariantAlarm,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::exception::ExceptionRouter;
use crate::infra::storage::entity::{
    account_balance, journal_entry, journal_line, refund, tax_subbalance, unallocated_balance,
};
use crate::infra::storage::repo::PendingQueueRepo;
use time::OffsetDateTime;

/// The allocation deferred-apply queue flow this job ages — the
/// `PAYMENT_ALLOCATE` literal (kept in lockstep with `SourceDocType::PaymentAllocate`
/// and the same literal `QueueApplierJob` sweeps).
const FLOW_PAYMENT_ALLOCATE: &str = "PAYMENT_ALLOCATE";

/// The chargeback deferred-apply queue flow this job ages — the `CHARGEBACK`
/// literal (kept in lockstep with `SourceDocType::Chargeback`).
const FLOW_CHARGEBACK: &str = "CHARGEBACK";

/// `UNALLOCATED` account-class code (matches `bss_ledger_sdk::AccountClass::Unallocated`).
const CLASS_UNALLOCATED: &str = "UNALLOCATED";

/// `REFUND_CLEARING` account-class code (matches
/// `bss_ledger_sdk::AccountClass::RefundClearing`) — the two-stage refund's stage-1
/// clearing liability this job ages (Slice 3 §4.4 / Group F).
const CLASS_REFUND_CLEARING: &str = "REFUND_CLEARING";

/// The `refund.phase` literal for a stage-1 initiation (matches
/// `RefundPhase::Initiated::as_str`) — the orphan scan looks for these with no
/// matching terminal phase.
const PHASE_INITIATED: &str = "initiated";

/// Refund-clearing aging WARN threshold (7 days, design §4.4 / §13 — "7 d Warn").
/// A `REFUND_CLEARING` balance open longer than this raises the `Warn`
/// `REFUND_CLEARING_AGED` alarm; a stage-1 refund unmatched this long also pages
/// Revenue Assurance (`STAGE1_REFUND_ORPHAN`). A documented const (mirrors
/// [`AGED_THRESHOLD_SECS`]; wire to `jobs.refund_clearing_warn_secs` for
/// per-deployment tuning when needed — deferred).
const REFUND_CLEARING_WARN_SECS: i64 = 7 * 24 * 60 * 60;

/// Refund-clearing aging PAGE threshold (14 days, design §4.4 / §13 — "14 d
/// Page"). A `REFUND_CLEARING` balance open longer than this escalates the 7-day
/// Warn to the `Critical` `STUCK_REFUND_CLEARING` close-blocking exception (+ the
/// `// exception stub (full exception_queue is Slice 7)` marker).
const REFUND_CLEARING_PAGE_SECS: i64 = 14 * 24 * 60 * 60;

/// Age threshold (seconds) past which a `QUEUED` row or a parked unallocated grain
/// trips its aged alarm. A documented const for now (the sibling `TieOutJob` reads
/// no config either; the cadences in `JobsConfig` are tick intervals, not domain
/// thresholds). 24h — generous relative to the few-minutes queue-drain cadence, so
/// only genuinely stuck work alarms. Wire to `jobs.aged_*_secs` config when the
/// thresholds need per-deployment tuning (deferred — §6).
const AGED_THRESHOLD_SECS: i64 = 86_400;

/// Upper bound on the cross-tenant aged-queue candidate read per tick — a ceiling
/// on how many `QUEUED` rows one pass loads into memory (mirrors the sweep job's
/// discovery limit). The aged subset is filtered from this candidate set by
/// `queued_at`.
const AGED_DISCOVERY_LIMIT: u64 = 10_000;

/// Cap on the per-alarm `affected` list — bounds the event size on a wide aged
/// backlog while still naming enough items for an operator to act (mirrors
/// `TieOutJob::MAX_AFFECTED`).
const MAX_AFFECTED: usize = 50;

/// One aged `QUEUED` queue row that tripped a queue-age alarm (ids only — no PII).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgedQueueItem {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The queue business id (the dedup/correlation key).
    pub business_id: String,
    /// Age of the row in whole seconds at scan time.
    pub age_secs: i64,
}

/// One unallocated grain still holding cash whose oldest contributing line is
/// older than the threshold (ids only — no PII).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgedUnallocatedGrain {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Paying tenant (the unallocated-pool owner dimension).
    pub payer_tenant_id: Uuid,
    /// Unallocated account whose pool is parked.
    pub account_id: Uuid,
    /// Grain currency.
    pub currency: String,
    /// Cached parked balance (`> 0`).
    pub balance_minor: i64,
    /// Age of the oldest contributing `UNALLOCATED` line in whole seconds.
    pub age_secs: i64,
}

/// One open `REFUND_CLEARING` balance grain whose oldest contributing line is
/// older than the refund-clearing aging threshold (ids only — no PII).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgedRefundClearingGrain {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The clearing account whose balance is stuck open.
    pub account_id: Uuid,
    /// Grain currency.
    pub currency: String,
    /// Cached open clearing balance (`> 0`).
    pub balance_minor: i64,
    /// Age of the oldest contributing `REFUND_CLEARING` line in whole seconds.
    pub age_secs: i64,
    /// `true` once the grain has aged past the 14-day PAGE threshold (the
    /// close-blocking `STUCK_REFUND_CLEARING` exception); `false` for a 7-day Warn.
    pub paged: bool,
}

/// One stage-1 refund (`initiated`) with no matching stage-2 / reversal beyond the
/// aging threshold — paged to Revenue Assurance (ids only — no PII).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage1OrphanRefund {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The PSP refund id whose stage-1 never advanced.
    pub psp_refund_id: String,
    /// Grain currency.
    pub currency: String,
    /// The stuck stage-1 amount in minor units.
    pub amount_minor: i64,
    /// Age of the stage-1 `refund` row in whole seconds.
    pub age_secs: i64,
}

/// One tax sub-balance that went negative in a CLOSED (prior) filing period —
/// negative BEYOND its filing window (design §4.5 / AC #17). An in-window
/// negative is a legitimate reversal and is NOT flagged; only a strictly-earlier
/// filing period that is negative pages Revenue Assurance (ids + amount only —
/// no PII).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NegativeTaxGrain {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The account whose per-jurisdiction tax cache went negative.
    pub account_id: Uuid,
    /// The tax jurisdiction dimension of the grain.
    pub tax_jurisdiction: String,
    /// The (closed, prior) filing period that is negative (`YYYYMM`).
    pub tax_filing_period: String,
    /// The negative cached tax balance in minor units (`< 0`).
    pub balance_minor: i64,
    /// Grain currency. The `tax_subbalance` cache carries no currency column, so
    /// this is always empty (the defect has no single currency — mirrors the
    /// other multi-/no-currency alarms).
    pub currency: String,
}

/// Whether a `tax_subbalance`'s `filing_period` is BEYOND its filing window —
/// i.e. strictly earlier than `current_period` (a CLOSED, prior period). Both
/// are `YYYYMM` strings, so a lexicographic `<` is a chronological comparison.
/// An in-window (current-period) negative is a legitimate reversal and returns
/// `false`; a future period (clock skew) also returns `false`. Factored out so
/// the window filter is unit-testable without a database.
#[must_use]
fn is_beyond_filing_window(filing_period: &str, current_period: &str) -> bool {
    filing_period < current_period
}

/// Periodic aged-alarm job over every tenant with queued work or parked cash.
pub struct AgedAlarmJob {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    /// Metrics sink (Group F): the refund-clearing balance/age gauges
    /// (`ledger_refund_clearing_balance_minor` / `_aged_seconds`) and the
    /// stage-1-orphan counter (`ledger_stage1_refund_orphan_total`, design §9).
    /// Defaults to the no-op so the queue/unallocated families need no metrics.
    metrics: Arc<dyn crate::domain::ports::metrics::LedgerMetricsPort>,
    // Slice 7 Phase 2: routes the 14-day-page `STUCK_REFUND_CLEARING` stub to a
    // durable close-blocking exception row (ADDITIVE beside the alarm). `None` until
    // `with_exceptions` wires it (so existing constructions are unchanged).
    exceptions: Option<Arc<ExceptionRouter>>,
}

impl AgedAlarmJob {
    /// Build the job over one database provider and the event publisher (used
    /// out-of-band to emit the aged alarms on a separate connection). The metrics
    /// sink defaults to the no-op (the queue / unallocated aging families emit no
    /// metrics); override via [`Self::with_metrics`] to feed the §9 refund-clearing
    /// gauges + stage-1-orphan counter.
    #[must_use]
    pub fn new(db: DBProvider<DbError>, publisher: Arc<LedgerEventPublisher>) -> Self {
        Self {
            db,
            publisher,
            metrics: Arc::new(crate::domain::ports::metrics::NoopLedgerMetrics),
            exceptions: None,
        }
    }

    /// Attach the exception router (Slice 7 Phase 2) so the 14-day-page
    /// `STUCK_REFUND_CLEARING` alarm also opens a durable close-blocking exception
    /// row. Additive — the existing alarm is unchanged.
    #[must_use]
    pub fn with_exceptions(mut self, exceptions: Arc<ExceptionRouter>) -> Self {
        self.exceptions = Some(exceptions);
        self
    }

    /// Bind the metrics sink (Group F): the refund-clearing balance/age gauges +
    /// the stage-1-orphan counter (design §9). Builder form (defaults to the no-op
    /// at `new`) so the existing `(db, publisher)` call sites stay source-compatible.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn crate::domain::ports::metrics::LedgerMetricsPort>,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// Run one aged-alarm pass: scan both queue flows + the unallocated pool
    /// across all tenants, emitting one `Warn` alarm per non-empty aged class.
    ///
    /// # Errors
    /// Returns `Err` only on an infrastructure failure reading the cross-tenant
    /// candidate feeds (the pass cannot start); per-tenant unallocated read faults
    /// are isolated (logged) within the pass.
    pub async fn run(&self) -> anyhow::Result<()> {
        let now = OffsetDateTime::now_utc();
        let threshold = Duration::seconds(AGED_THRESHOLD_SECS);

        // Z10-1: each alarm FAMILY runs independently. An infra fault enumerating one
        // family's candidate feed is logged and the pass continues to the others (a
        // single blip must not transiently blind the later scans for the whole tick —
        // they self-heal next tick). Per-tenant faults are already isolated INSIDE each
        // scan; this isolates the cross-family enumeration too.

        // --- Aged queue rows (both flows), grouped by tenant for one alarm each.
        match self
            .aged_queue_rows(FLOW_PAYMENT_ALLOCATE, now, threshold)
            .await
        {
            Ok(rows) => {
                self.emit_queue_alarms(AlarmCategory::AgedAllocationQueue, &rows)
                    .await;
            }
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: aged allocation-queue scan failed (infra); continuing"),
        }
        match self.aged_queue_rows(FLOW_CHARGEBACK, now, threshold).await {
            Ok(rows) => {
                self.emit_queue_alarms(AlarmCategory::DisputePhaseQueued, &rows)
                    .await;
            }
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: aged chargeback-queue scan failed (infra); continuing"),
        }

        // --- Aged unallocated grains, per tenant (isolated failures).
        match self.aged_unallocated_grains(now, threshold).await {
            Ok(rows) => self.emit_unallocated_alarms(&rows).await,
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: aged unallocated scan failed (infra); continuing"),
        }

        // --- Slice-3 Phase-2 (Group F): refund-clearing aging + stage-1 orphans.
        //     Distinct thresholds (7d Warn / 14d Page, §13) from the 24h queue/
        //     unallocated aging, so this scans on its own cutoffs.
        match self.aged_refund_clearing_grains(now).await {
            Ok(rows) => self.emit_refund_clearing_alarms(&rows).await,
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: aged refund-clearing scan failed (infra); continuing"),
        }
        match self.stage1_orphan_refunds(now).await {
            Ok(rows) => self.emit_stage1_orphan_alarms(&rows).await,
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: stage-1 orphan-refund scan failed (infra); continuing"),
        }

        // --- Slice-3 Phase-3 (Group 2): tax sub-balances negative beyond their filing
        //     window (design §4.5 / AC #17). In-window negatives are a legitimate reversal;
        //     only a CLOSED (prior) filing period that is negative pages Revenue Assurance.
        match self.negative_tax_subbalances(now).await {
            Ok(rows) => self.emit_negative_tax_alarms(&rows).await,
            Err(e) => tracing::error!(error = %e,
                "bss-ledger: negative-tax-subbalance scan failed (infra); continuing"),
        }

        Ok(())
    }

    /// Read the UNSCOPED cross-tenant `QUEUED` candidate feed for `flow` and keep
    /// only the rows whose `queued_at` is older than `threshold`. Mirrors the
    /// sweep job's `list_all_due` candidate-feed read (system-context).
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure reading the candidate feed.
    async fn aged_queue_rows(
        &self,
        flow: &str,
        now: OffsetDateTime,
        threshold: Duration,
    ) -> anyhow::Result<Vec<AgedQueueItem>> {
        let repo = PendingQueueRepo::new(self.db.clone());
        // `list_all_due` returns due `QUEUED` rows oldest-`queued_at` first; an
        // aged row is necessarily due (its `apply_after`, if any, is long past), so
        // the due feed is a superset of the aged set — filter it by `queued_at`.
        let rows = repo
            .list_all_due(flow, now, AGED_DISCOVERY_LIMIT)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: list_all_due({flow}): {e}"))?;
        let cutoff = now - threshold;
        Ok(rows
            .into_iter()
            .filter(|r| r.queued_at < cutoff)
            .map(|r| AgedQueueItem {
                tenant_id: r.tenant_id,
                business_id: r.business_id,
                age_secs: (now - r.queued_at).whole_seconds(),
            })
            .collect())
    }

    /// Scan every tenant's `UNALLOCATED` journal lines + `unallocated_balance`
    /// cache and flag grains whose oldest contributing line is older than
    /// `threshold` AND whose cached `balance_minor > 0`. Per-tenant failures are
    /// isolated (logged, the pass continues).
    ///
    /// # Errors
    /// Returns `Err` only if the up-front tenant enumeration fails.
    async fn aged_unallocated_grains(
        &self,
        now: OffsetDateTime,
        threshold: Duration,
    ) -> anyhow::Result<Vec<AgedUnallocatedGrain>> {
        // Enumerate tenants holding a parked unallocated grain (UNSCOPED system
        // scope). Scoped to a block so the connection is released before the
        // per-tenant loop opens its own.
        let tenant_ids: BTreeSet<Uuid> = {
            let conn = self.db.conn()?;
            let cache = unallocated_balance::Entity::find()
                .secure()
                .scope_with(&AccessScope::allow_all())
                .filter(Condition::all().add(unallocated_balance::Column::BalanceMinor.gt(0)))
                .all(&conn)
                .await
                .map_err(|e| anyhow::anyhow!("aged-alarms: enumerate unallocated tenants: {e}"))?;
            cache.iter().map(|c| c.tenant_id).collect()
        };

        let cutoff = now - threshold;
        let mut aged = Vec::new();
        for tenant_id in tenant_ids {
            match self
                .aged_unallocated_for_tenant(tenant_id, now, cutoff)
                .await
            {
                Ok(mut grains) => aged.append(&mut grains),
                Err(e) => {
                    // Isolate per-tenant infra faults: log and continue.
                    tracing::error!(
                        tenant_id = %tenant_id,
                        error = %e,
                        "bss-ledger: aged-unallocated scan failed for tenant; continuing"
                    );
                }
            }
        }
        Ok(aged)
    }

    /// Per-tenant unallocated age scan: read the tenant's journal entries (for the
    /// `entry_id -> posted_at_utc` map), its `UNALLOCATED` journal lines, and its
    /// `unallocated_balance` cache (all scoped), then flag grains older than
    /// `cutoff` with a positive cached balance.
    async fn aged_unallocated_for_tenant(
        &self,
        tenant_id: Uuid,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> anyhow::Result<Vec<AgedUnallocatedGrain>> {
        let conn = self.db.conn()?;
        let scope = AccessScope::for_tenant(tenant_id);

        let entries = journal_entry::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(journal_entry::Column::TenantId.eq(tenant_id)))
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read journal_entry: {e}"))?;
        let lines = journal_line::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(journal_line::Column::TenantId.eq(tenant_id))
                    .add(journal_line::Column::AccountClass.eq(CLASS_UNALLOCATED)),
            )
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read journal_line: {e}"))?;
        let cache = unallocated_balance::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(unallocated_balance::Column::TenantId.eq(tenant_id)))
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read unallocated_balance: {e}"))?;

        Ok(aged_grains(&entries, &lines, &cache, now, cutoff))
    }

    /// Scan every tenant's open `REFUND_CLEARING` balances (Group F, design §4.4):
    /// flag a grain whose oldest contributing `REFUND_CLEARING` line is older than
    /// the 7-day WARN threshold AND whose cached `balance_minor > 0` (the clearing
    /// is still open). Grains older than the 14-day PAGE threshold are marked
    /// `paged` (the `STUCK_REFUND_CLEARING` close-blocking escalation). Enumerates
    /// tenants from `account_balance` (UNSCOPED `allow_all`), re-reads each scoped;
    /// per-tenant faults are isolated.
    ///
    /// # Errors
    /// Returns `Err` only if the up-front tenant enumeration fails.
    async fn aged_refund_clearing_grains(
        &self,
        now: OffsetDateTime,
    ) -> anyhow::Result<Vec<AgedRefundClearingGrain>> {
        // Enumerate tenants holding an open REFUND_CLEARING balance (system scope).
        let tenant_ids: BTreeSet<Uuid> = {
            let conn = self.db.conn()?;
            let rows = account_balance::Entity::find()
                .secure()
                .scope_with(&AccessScope::allow_all())
                .filter(
                    Condition::all()
                        .add(account_balance::Column::AccountClass.eq(CLASS_REFUND_CLEARING))
                        .add(account_balance::Column::BalanceMinor.gt(0)),
                )
                .all(&conn)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("aged-alarms: enumerate refund-clearing tenants: {e}")
                })?;
            rows.iter().map(|c| c.tenant_id).collect()
        };

        let warn_cutoff = now - Duration::seconds(REFUND_CLEARING_WARN_SECS);
        let page_cutoff = now - Duration::seconds(REFUND_CLEARING_PAGE_SECS);
        let mut aged = Vec::new();
        for tenant_id in tenant_ids {
            match self
                .aged_refund_clearing_for_tenant(tenant_id, now, warn_cutoff, page_cutoff)
                .await
            {
                Ok(mut grains) => aged.append(&mut grains),
                Err(e) => tracing::error!(
                    tenant_id = %tenant_id, error = %e,
                    "bss-ledger: aged-refund-clearing scan failed for tenant; continuing"
                ),
            }
        }
        Ok(aged)
    }

    /// Per-tenant refund-clearing age scan: read the tenant's journal entries (for
    /// the `entry_id -> posted_at_utc` map), its `REFUND_CLEARING` journal lines,
    /// and its `account_balance` `REFUND_CLEARING` grains (all scoped), then flag
    /// grains whose oldest line is older than the WARN cutoff with a positive
    /// cached balance — marking `paged` when older than the PAGE cutoff. Also feeds
    /// the §9 balance/age gauges per tenant.
    async fn aged_refund_clearing_for_tenant(
        &self,
        tenant_id: Uuid,
        now: OffsetDateTime,
        warn_cutoff: OffsetDateTime,
        page_cutoff: OffsetDateTime,
    ) -> anyhow::Result<Vec<AgedRefundClearingGrain>> {
        let conn = self.db.conn()?;
        let scope = AccessScope::for_tenant(tenant_id);

        let entries = journal_entry::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(journal_entry::Column::TenantId.eq(tenant_id)))
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read journal_entry (clearing): {e}"))?;
        let lines = journal_line::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(journal_line::Column::TenantId.eq(tenant_id))
                    .add(journal_line::Column::AccountClass.eq(CLASS_REFUND_CLEARING)),
            )
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read journal_line (clearing): {e}"))?;
        let cache = account_balance::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(account_balance::Column::TenantId.eq(tenant_id))
                    .add(account_balance::Column::AccountClass.eq(CLASS_REFUND_CLEARING)),
            )
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read account_balance (clearing): {e}"))?;

        Ok(aged_refund_clearing_grains(
            tenant_id,
            &entries,
            &lines,
            &cache,
            now,
            warn_cutoff,
            page_cutoff,
            &*self.metrics,
        ))
    }

    /// Scan every tenant's stage-1 (`initiated`) refunds with no matching terminal
    /// phase (`confirmed` / `rejected` / `voided` / `unknown_final`) for the same
    /// `psp_refund_id`, aged past the WARN threshold (design §4.4 — "a stage-1
    /// entry without a matching stage-2 or stage-1 reversal beyond the threshold
    /// pages Revenue Assurance"). Enumerates tenants from the `refund` table
    /// (UNSCOPED `allow_all`), re-reads each scoped; per-tenant faults are isolated.
    ///
    /// # Errors
    /// Returns `Err` only if the up-front tenant enumeration fails.
    async fn stage1_orphan_refunds(
        &self,
        now: OffsetDateTime,
    ) -> anyhow::Result<Vec<Stage1OrphanRefund>> {
        // Enumerate tenants with at least one stage-1 refund row (system scope).
        let tenant_ids: BTreeSet<Uuid> = {
            let conn = self.db.conn()?;
            let rows = refund::Entity::find()
                .secure()
                .scope_with(&AccessScope::allow_all())
                .filter(Condition::all().add(refund::Column::Phase.eq(PHASE_INITIATED)))
                .all(&conn)
                .await
                .map_err(|e| anyhow::anyhow!("aged-alarms: enumerate refund tenants: {e}"))?;
            rows.iter().map(|r| r.tenant_id).collect()
        };

        let cutoff = now - Duration::seconds(REFUND_CLEARING_WARN_SECS);
        let mut orphans = Vec::new();
        for tenant_id in tenant_ids {
            match self.stage1_orphans_for_tenant(tenant_id, now, cutoff).await {
                Ok(mut rows) => orphans.append(&mut rows),
                Err(e) => tracing::error!(
                    tenant_id = %tenant_id, error = %e,
                    "bss-ledger: stage1-orphan scan failed for tenant; continuing"
                ),
            }
        }
        Ok(orphans)
    }

    /// Per-tenant stage-1-orphan scan: read all of the tenant's `refund` rows
    /// (scoped), group by `psp_refund_id`, and flag a `psp_refund_id` whose ONLY
    /// row is the stage-1 `initiated` (no terminal phase landed) AND whose stage-1
    /// `created_at_utc` is older than `cutoff`.
    async fn stage1_orphans_for_tenant(
        &self,
        tenant_id: Uuid,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> anyhow::Result<Vec<Stage1OrphanRefund>> {
        let conn = self.db.conn()?;
        let scope = AccessScope::for_tenant(tenant_id);
        let rows = refund::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(refund::Column::TenantId.eq(tenant_id)))
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read refund rows: {e}"))?;
        Ok(stage1_orphans(tenant_id, &rows, now, cutoff))
    }

    /// Emit the refund-clearing aging alarms (Group F): one `Warn`
    /// `REFUND_CLEARING_AGED` per tenant with at least one 7-day-aged grain, and —
    /// for grains past the 14-day PAGE threshold — one `Critical`
    /// `STUCK_REFUND_CLEARING` per tenant (the close-blocking exception stub).
    async fn emit_refund_clearing_alarms(&self, aged: &[AgedRefundClearingGrain]) {
        let mut warn_by_tenant: HashMap<Uuid, Vec<&AgedRefundClearingGrain>> = HashMap::new();
        let mut page_by_tenant: HashMap<Uuid, Vec<&AgedRefundClearingGrain>> = HashMap::new();
        for g in aged {
            warn_by_tenant.entry(g.tenant_id).or_default().push(g);
            if g.paged {
                page_by_tenant.entry(g.tenant_id).or_default().push(g);
            }
        }

        // 7-day Warn: aged clearing latency (re-detected each tick until it drains).
        for (tenant_id, grains) in warn_by_tenant {
            let detail = format!(
                "tenant={tenant_id} category=REFUND_CLEARING_AGED aged_grains={}",
                grains.len()
            );
            tracing::warn!(
                tenant_id = %tenant_id, aged_grains = grains.len(),
                "bss-ledger: aged refund-clearing balances detected"
            );
            let affected = grains
                .iter()
                .map(|g| AffectedItem {
                    id: format!("account={}/age_secs={}", g.account_id, g.age_secs),
                    currency: g.currency.clone(),
                    expected_minor: 0,
                    actual_minor: g.balance_minor,
                })
                .take(MAX_AFFECTED)
                .collect();
            self.emit_with_severity(
                tenant_id,
                AlarmCategory::RefundClearingAged,
                AlarmSeverity::Warn,
                &detail,
                affected,
            )
            .await;
        }

        // 14-day Page: the close-blocking STUCK_REFUND_CLEARING exception (Critical).
        for (tenant_id, grains) in page_by_tenant {
            // exception stub (full exception_queue is Slice 7)
            let detail = format!(
                "tenant={tenant_id} category=STUCK_REFUND_CLEARING paged_grains={} \
                 (close-blocking; full exception_queue is Slice 7)",
                grains.len()
            );
            tracing::error!(
                tenant_id = %tenant_id, paged_grains = grains.len(),
                "bss-ledger: refund-clearing aged past the 14-day PAGE threshold — \
                 STUCK_REFUND_CLEARING (close-blocking; full exception_queue is Slice 7)"
            );
            let affected = grains
                .iter()
                .map(|g| AffectedItem {
                    id: format!("account={}/age_secs={}", g.account_id, g.age_secs),
                    currency: g.currency.clone(),
                    expected_minor: 0,
                    actual_minor: g.balance_minor,
                })
                .take(MAX_AFFECTED)
                .collect();
            self.emit_with_severity(
                tenant_id,
                AlarmCategory::StuckRefundClearing,
                AlarmSeverity::Critical,
                &detail,
                affected,
            )
            .await;

            // Slice 7 Phase 2: ADDITIVELY open a durable close-blocking exception row
            // beside the alarm above. One deduped OPEN row per tenant (fixed
            // business_ref) — the periodic scan re-detects the aged grains each tick,
            // and the router's `(tenant, type, business_ref)` OPEN dedup collapses
            // them to a single close-blocking row.
            if let Some(ex) = &self.exceptions {
                ex.route(
                    tenant_id,
                    crate::domain::exception::ExceptionType::StuckRefundClearing,
                    "refund-clearing-aged",
                    Some(serde_json::json!({ "paged_grains": grains.len() })),
                )
                .await;
            }
        }
    }

    /// Emit one `STAGE1_REFUND_ORPHAN` `Warn` alarm per tenant with at least one
    /// orphaned stage-1 refund (paged to Revenue Assurance), carrying the orphan
    /// psp-refund ids (capped) + their amounts/ages. Bumps
    /// `ledger_stage1_refund_orphan_total` per orphan (design §9).
    async fn emit_stage1_orphan_alarms(&self, orphans: &[Stage1OrphanRefund]) {
        let mut by_tenant: HashMap<Uuid, Vec<&Stage1OrphanRefund>> = HashMap::new();
        for o in orphans {
            by_tenant.entry(o.tenant_id).or_default().push(o);
        }
        for (tenant_id, items) in by_tenant {
            for _ in &items {
                self.metrics.stage1_refund_orphan();
            }
            let detail = format!(
                "tenant={tenant_id} category=STAGE1_REFUND_ORPHAN orphans={} \
                 (pages Revenue Assurance)",
                items.len()
            );
            tracing::warn!(
                tenant_id = %tenant_id, orphans = items.len(),
                "bss-ledger: stage-1 refund orphans detected — paging Revenue Assurance"
            );
            let affected = items
                .iter()
                .map(|o| AffectedItem {
                    id: format!("psp_refund:{}/age_secs={}", o.psp_refund_id, o.age_secs),
                    currency: o.currency.clone(),
                    expected_minor: 0,
                    actual_minor: o.amount_minor,
                })
                .take(MAX_AFFECTED)
                .collect();
            self.emit_with_severity(
                tenant_id,
                AlarmCategory::Stage1RefundOrphan,
                AlarmSeverity::Warn,
                &detail,
                affected,
            )
            .await;
        }
    }

    /// Scan every tenant's `tax_subbalance` cache (Group 2, design §4.5 / AC #17)
    /// for grains that went negative BEYOND their filing window: read all rows
    /// with `balance_minor < 0` (UNSCOPED `allow_all`, system context — mirrors the
    /// other cross-tenant enumerations), then filter IN RUST to those whose
    /// `tax_filing_period` is strictly earlier than the current `YYYYMM` filing
    /// period (an in-window negative is a legitimate reversal and is NOT flagged).
    /// The grain is self-contained (`balance_minor` + jurisdiction + filing-period),
    /// so no journal age computation is needed — unlike the refund-clearing /
    /// unallocated scans, "beyond window" is a pure string comparison.
    ///
    /// # Errors
    /// Returns `Err` on an infrastructure failure reading `tax_subbalance`.
    async fn negative_tax_subbalances(
        &self,
        now: OffsetDateTime,
    ) -> anyhow::Result<Vec<NegativeTaxGrain>> {
        let current_period = crate::domain::instant::yyyymm(now);
        let conn = self.db.conn()?;
        let rows = tax_subbalance::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(tax_subbalance::Column::BalanceMinor.lt(0)))
            .all(&conn)
            .await
            .map_err(|e| anyhow::anyhow!("aged-alarms: read tax_subbalance: {e}"))?;
        Ok(rows
            .into_iter()
            .filter(|r| is_beyond_filing_window(&r.tax_filing_period, &current_period))
            .map(|r| NegativeTaxGrain {
                tenant_id: r.tenant_id,
                account_id: r.account_id,
                tax_jurisdiction: r.tax_jurisdiction,
                tax_filing_period: r.tax_filing_period,
                balance_minor: r.balance_minor,
                // `tax_subbalance` has no currency column (the defect has no single
                // currency) — empty, like the other multi-/no-currency alarms.
                currency: String::new(),
            })
            .collect())
    }

    /// Emit one `Critical` `NEGATIVE_TAX_SUBBALANCE` alarm per negative-beyond-window
    /// tax grain (Group 2). Unlike the per-tenant-grouped aged families this emits
    /// per grain — each `(jurisdiction, filing-period)` discrepancy is a distinct
    /// reconciliation item Revenue Assurance triages. Re-detected each tick while the
    /// negative persists.
    async fn emit_negative_tax_alarms(&self, grains: &[NegativeTaxGrain]) {
        for g in grains {
            let detail = format!(
                "tax_subbalance negative beyond filing window: jurisdiction={} filing_period={} balance_minor={}",
                g.tax_jurisdiction, g.tax_filing_period, g.balance_minor
            );
            tracing::error!(
                tenant_id = %g.tenant_id,
                account_id = %g.account_id,
                jurisdiction = %g.tax_jurisdiction,
                filing_period = %g.tax_filing_period,
                balance_minor = g.balance_minor,
                "bss-ledger: tax sub-balance negative beyond its filing window — \
                 NEGATIVE_TAX_SUBBALANCE (Revenue Assurance must reconcile)"
            );
            let affected = vec![AffectedItem {
                id: format!(
                    "account:{}/jurisdiction:{}/filing:{}",
                    g.account_id, g.tax_jurisdiction, g.tax_filing_period
                ),
                currency: g.currency.clone(),
                expected_minor: 0,
                actual_minor: g.balance_minor,
            }];
            self.emit_with_severity(
                g.tenant_id,
                AlarmCategory::NegativeTaxSubbalance,
                AlarmSeverity::Critical,
                &detail,
                affected,
            )
            .await;
        }
    }

    /// Emit one fire-and-forget invariant alarm for `category` at `severity`
    /// against `tenant` — the severity-parameterized twin of [`Self::emit`] (which
    /// is hard-wired to `Warn`). Used by the refund-clearing path, where the 14-day
    /// `STUCK_REFUND_CLEARING` page is `Critical` (design §13).
    async fn emit_with_severity(
        &self,
        tenant_id: Uuid,
        category: AlarmCategory,
        severity: AlarmSeverity,
        detail: &str,
        affected: Vec<AffectedItem>,
    ) {
        let code = category.as_str().to_owned();
        let alarm = LedgerInvariantAlarm {
            category,
            severity,
            tenant_id,
            scope: format!("tenant:{tenant_id}"),
            code,
            detail: detail.to_owned(),
            affected,
        };
        self.publisher
            .emit_invariant_alarm(&SecurityContext::anonymous(), alarm)
            .await;
    }

    /// Emit one `Warn` alarm per tenant that has at least one aged queue row for
    /// `category`, carrying the aged business ids (capped) as `affected`.
    async fn emit_queue_alarms(&self, category: AlarmCategory, aged: &[AgedQueueItem]) {
        // Group the aged rows by tenant — one alarm per (tenant, category).
        let mut by_tenant: HashMap<Uuid, Vec<&AgedQueueItem>> = HashMap::new();
        for item in aged {
            by_tenant.entry(item.tenant_id).or_default().push(item);
        }
        for (tenant_id, items) in by_tenant {
            let detail = format!(
                "tenant={} category={} aged_rows={}",
                tenant_id,
                category.as_str(),
                items.len(),
            );
            tracing::warn!(
                tenant_id = %tenant_id,
                category = category.as_str(),
                aged_rows = items.len(),
                "bss-ledger: aged queue rows detected"
            );
            let affected = items
                .iter()
                .map(|i| AffectedItem {
                    id: i.business_id.clone(),
                    currency: String::new(),
                    // Age proxy: expected=0 (no threshold leg), actual=age in
                    // seconds (the divergence an operator reads).
                    expected_minor: 0,
                    actual_minor: i.age_secs,
                })
                .take(MAX_AFFECTED)
                .collect();
            self.emit(tenant_id, category, &detail, affected).await;
        }
    }

    /// Emit one `AGED_UNALLOCATED` `Warn` alarm per tenant with at least one aged
    /// parked grain, carrying the grain keys (capped) + their balances/ages.
    async fn emit_unallocated_alarms(&self, aged: &[AgedUnallocatedGrain]) {
        let mut by_tenant: HashMap<Uuid, Vec<&AgedUnallocatedGrain>> = HashMap::new();
        for g in aged {
            by_tenant.entry(g.tenant_id).or_default().push(g);
        }
        for (tenant_id, grains) in by_tenant {
            let detail = format!(
                "tenant={tenant_id} category=AGED_UNALLOCATED aged_grains={}",
                grains.len(),
            );
            tracing::warn!(
                tenant_id = %tenant_id,
                aged_grains = grains.len(),
                "bss-ledger: aged unallocated grains detected"
            );
            let affected = grains
                .iter()
                .map(|g| AffectedItem {
                    id: format!(
                        "payer={}/account={}/age_secs={}",
                        g.payer_tenant_id, g.account_id, g.age_secs
                    ),
                    currency: g.currency.clone(),
                    // expected=0 (no target), actual=the parked balance still sat
                    // in the pool (what an operator must get allocated/returned).
                    expected_minor: 0,
                    actual_minor: g.balance_minor,
                })
                .take(MAX_AFFECTED)
                .collect();
            self.emit(tenant_id, AlarmCategory::AgedUnallocated, &detail, affected)
                .await;
        }
    }

    /// Emit one fire-and-forget `Warn` invariant alarm for `category` against
    /// `tenant`. Mirrors [`crate::infra::jobs::tieout::TieOutJob::emit`] but at
    /// `Warn` (aged alarms flag latency, not a books defect).
    async fn emit(
        &self,
        tenant_id: Uuid,
        category: AlarmCategory,
        detail: &str,
        affected: Vec<AffectedItem>,
    ) {
        let code = category.as_str().to_owned();
        let alarm = LedgerInvariantAlarm {
            category,
            severity: AlarmSeverity::Warn,
            tenant_id,
            scope: format!("tenant:{tenant_id}"),
            code,
            detail: detail.to_owned(),
            affected,
        };
        self.publisher
            .emit_invariant_alarm(&SecurityContext::anonymous(), alarm)
            .await;
    }
}

/// Pure aged-unallocated detector (factored out so it is unit-testable without a
/// database): fold the `UNALLOCATED` lines into a per-grain MIN post time using
/// the `entry_id -> posted_at_utc` map, then flag a grain iff its oldest line is
/// older than `cutoff` AND the cache holds `balance_minor > 0`. The grain key
/// `(payer_tenant_id, account_id, currency)` mirrors
/// `BalanceProjector::derive_grains`' unallocated grain.
fn aged_grains(
    entries: &[journal_entry::Model],
    lines: &[journal_line::Model],
    cache: &[unallocated_balance::Model],
    now: OffsetDateTime,
    cutoff: OffsetDateTime,
) -> Vec<AgedUnallocatedGrain> {
    // entry_id -> posted_at_utc (the age source; `journal_line` carries none).
    let posted_at: HashMap<Uuid, OffsetDateTime> = entries
        .iter()
        .map(|e| (e.entry_id, e.posted_at_utc))
        .collect();

    // (payer_tenant_id, account_id, currency) -> oldest contributing post time.
    let mut oldest: HashMap<(Uuid, Uuid, String), OffsetDateTime> = HashMap::new();
    for line in lines {
        // A line whose entry isn't in the map can't be aged (defensive — entries
        // and lines are read in the same scope, so this should not happen).
        let Some(ts) = posted_at.get(&line.entry_id).copied() else {
            continue;
        };
        let key = (line.payer_tenant_id, line.account_id, line.currency.clone());
        oldest
            .entry(key)
            .and_modify(|cur| {
                if ts < *cur {
                    *cur = ts;
                }
            })
            .or_insert(ts);
    }

    // Flag a parked cache grain (`balance_minor > 0`) whose oldest line is aged.
    cache
        .iter()
        .filter(|c| c.balance_minor > 0)
        .filter_map(|c| {
            let key = (c.payer_tenant_id, c.account_id, c.currency.clone());
            let oldest_ts = *oldest.get(&key)?;
            (oldest_ts < cutoff).then(|| AgedUnallocatedGrain {
                tenant_id: c.tenant_id,
                payer_tenant_id: c.payer_tenant_id,
                account_id: c.account_id,
                currency: c.currency.clone(),
                balance_minor: c.balance_minor,
                age_secs: (now - oldest_ts).whole_seconds(),
            })
        })
        .collect()
}

/// Pure refund-clearing-aging detector (factored out so it is unit-testable
/// without a database, mirroring [`aged_grains`]): fold the `REFUND_CLEARING`
/// lines into a per-account MIN post time via the `entry_id -> posted_at_utc` map,
/// then flag an `account_balance` `REFUND_CLEARING` grain iff its oldest line is
/// older than `warn_cutoff` AND the cache holds `balance_minor > 0`. A grain whose
/// oldest line is also older than `page_cutoff` is marked `paged` (the
/// `STUCK_REFUND_CLEARING` 14-day escalation). Feeds the §9 balance/age gauges per
/// grain via `metrics` (a side effect kept here so the live + test paths agree).
/// The grain key is `(account_id, currency)` — a clearing account is a stream-less
/// system grain (no payer dimension, unlike the unallocated pool).
#[allow(clippy::too_many_arguments)]
fn aged_refund_clearing_grains(
    _tenant_id: Uuid,
    entries: &[journal_entry::Model],
    lines: &[journal_line::Model],
    cache: &[account_balance::Model],
    now: OffsetDateTime,
    warn_cutoff: OffsetDateTime,
    page_cutoff: OffsetDateTime,
    metrics: &dyn crate::domain::ports::metrics::LedgerMetricsPort,
) -> Vec<AgedRefundClearingGrain> {
    let posted_at: HashMap<Uuid, OffsetDateTime> = entries
        .iter()
        .map(|e| (e.entry_id, e.posted_at_utc))
        .collect();

    // (account_id, currency) -> oldest contributing REFUND_CLEARING post time.
    let mut oldest: HashMap<(Uuid, String), OffsetDateTime> = HashMap::new();
    for line in lines {
        let Some(ts) = posted_at.get(&line.entry_id).copied() else {
            continue;
        };
        let key = (line.account_id, line.currency.clone());
        oldest
            .entry(key)
            .and_modify(|cur| {
                if ts < *cur {
                    *cur = ts;
                }
            })
            .or_insert(ts);
    }

    cache
        .iter()
        .filter(|c| c.balance_minor > 0)
        .filter_map(|c| {
            let key = (c.account_id, c.currency.clone());
            let oldest_ts = *oldest.get(&key)?;
            // Feed the §9 gauges for every OPEN grain (not just aged ones): the
            // balance + its current age, by tenant.
            let age_secs = (now - oldest_ts).whole_seconds();
            metrics.refund_clearing_balance_minor(c.tenant_id, c.balance_minor);
            #[allow(clippy::cast_precision_loss)]
            metrics.refund_clearing_aged_seconds(c.tenant_id, age_secs as f64);
            (oldest_ts < warn_cutoff).then(|| AgedRefundClearingGrain {
                tenant_id: c.tenant_id,
                account_id: c.account_id,
                currency: c.currency.clone(),
                balance_minor: c.balance_minor,
                age_secs,
                paged: oldest_ts < page_cutoff,
            })
        })
        .collect()
}

/// Pure stage-1-orphan detector (factored out so it is unit-testable without a
/// database): group a tenant's `refund` rows by `psp_refund_id`, and flag a
/// `psp_refund_id` whose set of rows contains a stage-1 `initiated` but NO terminal
/// phase (`confirmed` / `rejected` / `voided` / `unknown_final`) AND whose stage-1
/// `created_at_utc` is older than `cutoff`. The stage-1 row carries the stuck
/// amount + currency. A stage-1 that already advanced (any non-`initiated` phase
/// row exists for the same `psp_refund_id`) is NOT an orphan.
fn stage1_orphans(
    tenant_id: Uuid,
    rows: &[refund::Model],
    now: OffsetDateTime,
    cutoff: OffsetDateTime,
) -> Vec<Stage1OrphanRefund> {
    // psp_refund_id -> (has a terminal/advanced phase?, the stage-1 row if present).
    let mut by_psp: HashMap<&str, (bool, Option<&refund::Model>)> = HashMap::new();
    for r in rows {
        let entry = by_psp
            .entry(r.psp_refund_id.as_str())
            .or_insert((false, None));
        if r.phase == PHASE_INITIATED {
            entry.1 = Some(r);
        } else {
            // Any non-initiated phase (confirmed / rejected / voided / unknown_final)
            // means the stage-1 advanced — not an orphan.
            entry.0 = true;
        }
    }

    by_psp
        .into_values()
        .filter_map(|(advanced, stage1)| {
            let stage1 = stage1?;
            if advanced || stage1.created_at_utc >= cutoff {
                return None;
            }
            Some(Stage1OrphanRefund {
                tenant_id,
                psp_refund_id: stage1.psp_refund_id.clone(),
                currency: stage1.currency.clone(),
                amount_minor: stage1.amount_minor,
                age_secs: (now - stage1.created_at_utc).whole_seconds(),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "aged_alarms_tests.rs"]
mod tests;
