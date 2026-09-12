//! `QueueApplierJob` — the periodic sweep that drains queued payment
//! allocations AND out-of-order chargeback phases (Slice 2b, Groups B–D).
//!
//! The drain-on-settle hook ([`crate::infra::payment::settle::SettlementService`])
//! applies a tenant's allocation queue right after a settle commits, but a queued
//! allocation whose settlement arrived through some OTHER path (or whose
//! drain-on-settle swallowed an error) still needs a backstop. This job is that
//! backstop: each tick it finds due `QUEUED` rows ACROSS ALL TENANTS for both the
//! `PAYMENT_ALLOCATE` flow (allocation-before-settlement) and the `CHARGEBACK`
//! flow (an out-of-order `won`/`lost` whose `opened` has since landed), then
//! drains each tenant under its own scope. A fresh `opened` also drains the
//! CHARGEBACK flow inline (`ChargebackService::record_phase`, mirroring
//! drain-on-settle); this periodic sweep is the backstop.
//!
//! ## System-context / cross-tenant (mirrors `period_open`)
//! Finding due rows is an UNSCOPED, cross-tenant read
//! ([`PendingQueueRepo::list_all_due`], the sanctioned system-context pattern of
//! [`crate::infra::storage::repo::ReferenceRepo::list_all_fiscal_calendars`]).
//! Each APPLY is then scoped per-tenant by `AccessScope::for_tenant(tenant)` via
//! the [`QueueApplier`] (which claims under `SKIP LOCKED` in its own txn, so the
//! cross-tenant list is only a candidate feed — a row another applier grabbed
//! first is simply skipped). The gauge read is likewise unscoped
//! ([`PendingQueueRepo::count_all_by_status`]).

use std::collections::BTreeSet;
use std::sync::Arc;

use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::payment::queue_apply::QueueApplier;
use crate::infra::storage::repo::PendingQueueRepo;
use time::OffsetDateTime;

/// The allocation deferred-apply queue flow this job sweeps — the
/// `PAYMENT_ALLOCATE` literal (kept in lockstep with
/// `SourceDocType::PaymentAllocate`; the same literal `AllocationService` stamps
/// on its dedup key + queue rows).
const FLOW_PAYMENT_ALLOCATE: &str = "PAYMENT_ALLOCATE";

/// The chargeback deferred-apply queue flow this job sweeps — the `CHARGEBACK`
/// literal (kept in lockstep with `SourceDocType::Chargeback`; the same literal
/// `ChargebackService` stamps on its dedup key + queue rows for an out-of-order
/// `won`/`lost`).
const FLOW_CHARGEBACK: &str = "CHARGEBACK";

/// The refund-of-refund claw-back deferred-apply queue flow this job sweeps — the
/// `REFUND_CLAWBACK` literal (the same literal `RefundHandler` stamps on its dedup
/// key + queue rows for an out-of-order / would-underflow claw-back, Group E). A
/// row here is a claw-back whose money-out decrement underflowed at intake; the
/// sweep re-tries it (its matching outbound refund stage-1 may have since landed)
/// and escalates one that aged out. Kept distinct from the engine `REFUND`
/// source-doc.
const FLOW_REFUND_CLAWBACK: &str = "REFUND_CLAWBACK";

/// The refund-quarantine deferred-de-quarantine queue flow this job sweeps — the
/// `REFUND_QUARANTINE` literal (the same literal `RefundHandler` stamps on a
/// refund-before-payment, Group G). A row here is a refund whose origin payment was
/// absent at intake; the sweep RE-VALIDATES it (the origin may have since landed)
/// and gates an over-D2 release to approval (never auto-posts). Distinct flow.
const FLOW_REFUND_QUARANTINE: &str = "REFUND_QUARANTINE";

/// The refund dispute-hold queue flow this job sweeps — the `REFUND_DISPUTE_HOLD`
/// literal (the same literal `RefundHandler` stamps on a refund whose origin payment
/// has an OPEN dispute, Z5-2 / design §5). A row here is a refund whose cash leg was
/// held while the dispute is sub judice; the sweep RE-READS the dispute (WON →
/// re-drive the gated post / open an approval over D2; LOST → cancel + escalate, the
/// chargeback already returned the money; still-OPEN → back off, aged out → cancel +
/// escalate). Distinct flow.
const FLOW_REFUND_DISPUTE_HOLD: &str = "REFUND_DISPUTE_HOLD";

/// `QUEUED` dedup/queue-row status — the rows this sweep targets (and gauges).
const STATUS_QUEUED: &str = "QUEUED";

/// Per-tenant cap on one sweep pass. A tenant with a backlog larger than this
/// drains the remainder on the next tick (the job is idempotent — already-applied
/// rows are terminal and never re-claimed). Generous relative to the
/// drain-on-settle cap so the sweep makes real progress on a backlog.
const SWEEP_PER_TENANT_CAP: u64 = 500;

/// Upper bound on the cross-tenant candidate list read per tick — a ceiling on
/// how many distinct tenants one pass discovers, so a pathological queue can't
/// load an unbounded candidate set into memory. (Distinct tenants ≤ this.)
const SWEEP_DISCOVERY_LIMIT: u64 = 10_000;

/// Outcome of one sweep pass (aggregated across every tenant drained).
#[derive(Debug, Default)]
pub struct QueueApplierReport {
    /// Distinct tenants that had at least one due queued row this pass.
    pub tenants_swept: u64,
    /// Rows that posted + flipped `→APPLIED` this pass.
    pub applied: u64,
    /// Rows left `QUEUED` because the payment is not yet settled.
    pub not_ready: u64,
    /// Rows left `QUEUED` after an apply-time cap/precondition rejection.
    pub blocked: u64,
}

/// Periodic sweep that drains due queued allocations across all tenants.
pub struct QueueApplierJob {
    db: DBProvider<DbError>,
    publisher: Arc<LedgerEventPublisher>,
    metrics: Arc<dyn LedgerMetricsPort>,
    /// The dual-control engine (Group G de-quarantine): threaded into the
    /// `QueueApplier` so an over-THEN-CURRENT-D2 refund de-quarantine routes to
    /// approval (never auto-posts). `None` ⇒ the de-quarantine drain is un-gated.
    approval: Option<Arc<crate::infra::approval::service::ApprovalService>>,
    /// Per-allocation touched-invoice cap (`payments` config), threaded into the
    /// `QueueApplier` so the deferred-apply drain uses the SAME cap as the inline
    /// path. Defaults to the allocate module's `MAX_INVOICES_PER_ALLOCATION`.
    max_invoices: usize,
}

impl QueueApplierJob {
    /// Build the job over one database provider, the event publisher (threaded
    /// into the posting engine the applier builds), and the metrics sink (for the
    /// queue-depth gauge). Mirrors [`crate::infra::jobs::period_open::PeriodOpenJob::new`]
    /// plus the publisher + metrics the apply path needs.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        Self {
            db,
            publisher,
            metrics,
            approval: None,
            max_invoices: crate::infra::payment::allocate::MAX_INVOICES_PER_ALLOCATION,
        }
    }

    /// Override the per-allocation touched-invoice cap (from `payments` config),
    /// threaded into the `QueueApplier`. Builder form; defaults to the allocate
    /// module's `MAX_INVOICES_PER_ALLOCATION`.
    #[must_use]
    pub fn with_max_invoices_per_allocation(mut self, max_invoices: usize) -> Self {
        self.max_invoices = max_invoices;
        self
    }

    /// Attach the dual-control engine (Group G): the de-quarantine drain then gates
    /// an over-THEN-CURRENT-D2 refund release to approval. Builder form (defaults to
    /// `None`).
    #[must_use]
    pub fn with_approval(
        mut self,
        approval: Arc<crate::infra::approval::service::ApprovalService>,
    ) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Drain due queued allocations across all tenants, then emit the queue-depth
    /// gauge. A per-tenant drain error is isolated (logged, the pass continues) so
    /// one flaky tenant doesn't abort the sweep — mirrors the tie-out job.
    ///
    /// # Errors
    /// Returns `Err` only on an infrastructure failure reading the cross-tenant
    /// candidate list (the pass cannot start); per-tenant drain faults are
    /// swallowed within the pass.
    pub async fn run(&self) -> anyhow::Result<QueueApplierReport> {
        let repo = PendingQueueRepo::new(self.db.clone());
        let now = OffsetDateTime::now_utc();

        // Cross-tenant candidate feed (UNSCOPED, system-context). Collapse to the
        // distinct tenant set — the per-tenant `drain` re-claims under SKIP LOCKED
        // (and caps at `SWEEP_PER_TENANT_CAP`), so we only need the tenant ids
        // here, not the rows themselves.
        let due = repo
            .list_all_due(FLOW_PAYMENT_ALLOCATE, now, SWEEP_DISCOVERY_LIMIT)
            .await?;
        let tenants: BTreeSet<Uuid> = due.into_iter().map(|r| r.tenant_id).collect();

        // System-context actor for the applied posts (the sweep is not a
        // per-request actor; the entry header's actor is stamped from this ctx).
        let ctx = SecurityContext::anonymous();
        let mut applier = QueueApplier::new(
            self.db.clone(),
            Arc::clone(&self.publisher),
            Arc::clone(&self.metrics),
        )
        .with_max_invoices_per_allocation(self.max_invoices);
        // Gate the de-quarantine drain over the THEN-CURRENT D2 threshold (Group G).
        if let Some(approval) = &self.approval {
            applier = applier.with_approval(Arc::clone(approval));
        }

        let mut report = QueueApplierReport::default();
        let mut swept: BTreeSet<Uuid> = BTreeSet::new();
        for tenant in tenants {
            let scope = AccessScope::for_tenant(tenant);
            match applier
                .drain(&ctx, &scope, tenant, SWEEP_PER_TENANT_CAP)
                .await
            {
                Ok(drain) => {
                    swept.insert(tenant);
                    report.applied += drain.applied;
                    report.not_ready += drain.not_ready;
                    report.blocked += drain.blocked;
                }
                Err(e) => {
                    // Isolate per-tenant infra faults: log and continue.
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: queue-applier sweep failed for tenant; continuing"
                    );
                }
            }
        }

        // CHARGEBACK flow (out-of-order won/lost whose opened has since landed) —
        // the same cross-tenant candidate feed + per-tenant scoped drain shape as
        // the allocation flow. Aggregated into the same report counters; a tenant
        // is counted once even if it had both flows queued (the `swept` set).
        let cb_due = repo
            .list_all_due(FLOW_CHARGEBACK, now, SWEEP_DISCOVERY_LIMIT)
            .await?;
        let cb_tenants: BTreeSet<Uuid> = cb_due.into_iter().map(|r| r.tenant_id).collect();
        for tenant in cb_tenants {
            let scope = AccessScope::for_tenant(tenant);
            match applier
                .drain_chargeback(&ctx, &scope, tenant, SWEEP_PER_TENANT_CAP)
                .await
            {
                Ok(drain) => {
                    swept.insert(tenant);
                    report.applied += drain.applied;
                    report.not_ready += drain.not_ready;
                    report.blocked += drain.blocked;
                }
                Err(e) => {
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: chargeback queue-applier sweep failed for tenant; continuing"
                    );
                }
            }
        }

        // REFUND_CLAWBACK flow (deferred refund-of-refund claw-backs whose matching
        // outbound refund stage-1 may have since landed) — the same cross-tenant
        // candidate feed + per-tenant scoped drain shape (Group E). A reconciled
        // claw-back posts (APPLIED); one that aged out without reconciling is
        // CANCELLED + escalated (the alarm fires inside the drain). Aggregated into
        // the same report counters (a still-deferred / escalated row counts as
        // neither applied nor not_ready — only `applied` is reflected here, mirroring
        // how the report aggregates the other flows' applied totals).
        let cb_clawback_due = repo
            .list_all_due(FLOW_REFUND_CLAWBACK, now, SWEEP_DISCOVERY_LIMIT)
            .await?;
        let clawback_tenants: BTreeSet<Uuid> =
            cb_clawback_due.into_iter().map(|r| r.tenant_id).collect();
        for tenant in clawback_tenants {
            let scope = AccessScope::for_tenant(tenant);
            match applier
                .drain_clawback(&ctx, &scope, tenant, SWEEP_PER_TENANT_CAP)
                .await
            {
                Ok(drain) => {
                    swept.insert(tenant);
                    report.applied += drain.applied;
                }
                Err(e) => {
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: claw-back queue-applier sweep failed for tenant; continuing"
                    );
                }
            }
        }
        // REFUND_QUARANTINE flow (refund-before-payment whose origin may have since
        // landed) — the de-quarantine sweep (Group G). Same cross-tenant feed +
        // per-tenant scoped GATED drain shape. A released refund posts (or opens an
        // approval if over the THEN-CURRENT D2 — never auto-posts); an origin that
        // never lands ages out → CANCELLED + escalate. Aggregated `applied` reflects
        // only the released-and-posted rows.
        let q_due = repo
            .list_all_due(FLOW_REFUND_QUARANTINE, now, SWEEP_DISCOVERY_LIMIT)
            .await?;
        let q_tenants: BTreeSet<Uuid> = q_due.into_iter().map(|r| r.tenant_id).collect();
        for tenant in q_tenants {
            let scope = AccessScope::for_tenant(tenant);
            match applier
                .drain_quarantine(&ctx, &scope, tenant, SWEEP_PER_TENANT_CAP)
                .await
            {
                Ok(drain) => {
                    swept.insert(tenant);
                    report.applied += drain.released;
                }
                Err(e) => {
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: refund-quarantine de-quarantine sweep failed for tenant; \
                         continuing"
                    );
                }
            }
        }
        // REFUND_DISPUTE_HOLD flow (refund whose origin payment has an OPEN dispute,
        // whose dispute may have since resolved) — the dispute-hold drain (Z5-2 /
        // design §5). Same cross-tenant feed + per-tenant scoped GATED drain shape. A
        // WON dispute re-drives the gated post (or opens an approval if over the
        // THEN-CURRENT D2 — never auto-posts); a LOST dispute cancels the hold (the
        // chargeback already returned the money — posting would double-pay); a dispute
        // that never resolves ages out → CANCELLED + escalate. Aggregated `applied`
        // reflects only the released-and-posted rows.
        let dh_due = repo
            .list_all_due(FLOW_REFUND_DISPUTE_HOLD, now, SWEEP_DISCOVERY_LIMIT)
            .await?;
        let dh_tenants: BTreeSet<Uuid> = dh_due.into_iter().map(|r| r.tenant_id).collect();
        for tenant in dh_tenants {
            let scope = AccessScope::for_tenant(tenant);
            match applier
                .drain_dispute_hold(&ctx, &scope, tenant, SWEEP_PER_TENANT_CAP)
                .await
            {
                Ok(drain) => {
                    swept.insert(tenant);
                    report.applied += drain.released;
                }
                Err(e) => {
                    tracing::error!(
                        tenant_id = %tenant,
                        error = %e,
                        "bss-ledger: refund dispute-hold sweep failed for tenant; continuing"
                    );
                }
            }
        }
        report.tenants_swept = u64::try_from(swept.len()).unwrap_or(u64::MAX);

        // Queue-depth gauge: the live cross-tenant backlog of still-`QUEUED`
        // allocations AFTER this pass (a count failure is non-fatal — log + skip
        // the emit; the sweep itself succeeded). The gauge tracks the allocation
        // flow (the dispute-phase queue depth has no dedicated gauge in this phase).
        match repo
            .count_all_by_status(FLOW_PAYMENT_ALLOCATE, STATUS_QUEUED)
            .await
        {
            Ok(depth) => self.metrics.allocation_queue_depth(depth),
            Err(e) => tracing::warn!(
                error = %e,
                "bss-ledger: queue-applier failed to read queue depth for gauge"
            ),
        }

        // Refund-quarantine depth gauge (`ledger_refund_quarantine_depth`, §9 /
        // Group G): the live cross-tenant backlog of still-`QUEUED` quarantined
        // refunds AFTER this pass. Non-fatal on a count failure.
        match repo
            .count_all_by_status(FLOW_REFUND_QUARANTINE, STATUS_QUEUED)
            .await
        {
            Ok(depth) => self.metrics.refund_quarantine_depth(depth),
            Err(e) => tracing::warn!(
                error = %e,
                "bss-ledger: queue-applier failed to read refund-quarantine depth for gauge"
            ),
        }

        Ok(report)
    }
}
