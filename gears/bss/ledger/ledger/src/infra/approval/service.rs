//! `ApprovalService` — the dual-control lifecycle engine (VHP-1852). Owns the
//! `PENDING → APPROVED | REJECTED | NEEDS_REWORK | CANCELLED | EXPIRED` state
//! machine, the `preparer ≠ approver` rule, and the same-transaction decision
//! audit. It dispatches the governed mutation through an [`ApprovalExecutor`]
//! port so the lifecycle stays testable in isolation from the posting engine.
//!
//! **Latch-execute-mark (DC-impl, H2).** `PostingService` opens its own
//! serializable transaction, so `approve` cannot nest the mutation inside the
//! approval transaction. To keep the dual-control invariant "a rejected/cancelled
//! approval never executes", `approve` (0) atomically latches `PENDING →
//! APPROVING` in its own txn — once latched, `reject`/`cancel`/`request-changes`
//! (all keyed on the `PENDING` state) can no longer win; then (1) executes the
//! stored `intent` through the executor (an idempotent mutation in its own txn);
//! then (2) marks `APPROVING → APPROVED` + writes the decision audit. If the
//! mutation fails (its txn rolled back, nothing committed) the latch is reverted
//! to `PENDING` so the approval is actionable again. Idempotency covers the crash
//! windows: a crash after the latch (before/during/after execute, before the
//! mark) leaves the row `APPROVING`; a later approve recovers it — re-running the
//! mutation (the idempotency key short-circuits a committed post) and completing
//! the mark. Without the latch, a concurrent `reject` landing during execute would
//! leave the mutation committed but the approval `REJECTED`.
//!
//! **Audit (DC7).** No `secured_audit_record` writer exists on this base (Slice 6
//! brings it). The decision audit is recorded in the same transaction as the
//! state transition via the append-only `ledger_approval_comment` thread, carrying
//! a structured JSON body. When Slice 6 lands, add the `secured_audit_record`
//! write alongside this call in the same txn.

use std::sync::Arc;

use bss_ledger_sdk::SourceDocType;

use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, TxConfig, is_unique_violation};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::FxConfig;
use crate::domain::approval::ApprovalState;
use crate::domain::approval::intent::ApprovalIntent;
use crate::domain::approval::policy::{
    DualControlPolicy, OperationFacts, PolicyConfigError, PolicyVersion, effective_version,
    requires_dual_control, resolve_policy, validate_config,
};
use crate::domain::error::DomainError;
use crate::domain::fx::translate::translate_amount;
use crate::domain::instant::format_rfc3339;
use crate::domain::instant::to_naive_date;
use crate::domain::model::RepoError;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::fx::rate_source::RateSource;
use crate::infra::storage::repo::{
    ApprovalRepo, FxRepo, JournalRepo, NewPendingApproval, NewPolicyVersion, ReferenceRepo,
};
use time::Duration;
use time::OffsetDateTime;

/// The seam through which an approved governed mutation is actually executed.
/// `ApprovalService` reconstructs the [`ApprovalIntent`] from the stored row and
/// hands it here; the concrete adapter (wired in `module`) replays the inline
/// flow (reverse / credit-grant / chargeback) idempotently. Kept a port so the
/// lifecycle engine is unit-testable with a stub.
#[async_trait::async_trait]
pub trait ApprovalExecutor: Send + Sync {
    /// Execute the governed mutation captured by `intent`, idempotently (a replay
    /// short-circuits via the foundation idempotency key).
    ///
    /// # Errors
    /// A [`DomainError`] propagates the mutation's own rejection unchanged.
    async fn execute(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: &ApprovalIntent,
    ) -> Result<(), DomainError>;
}

/// Dual-control lifecycle engine. Cheap to clone (`Arc` + handle fields).
#[derive(Clone)]
pub struct ApprovalService {
    db: DBProvider<DbError>,
    repo: ApprovalRepo,
    executor: Arc<dyn ApprovalExecutor>,
    metrics: Arc<dyn LedgerMetricsPort>,
    // DC10 / FX: the dual-control threshold is held in the tenant's FUNCTIONAL
    // (reporting) currency, so the gate translates a cross-currency comparand into
    // it before the threshold compare. `source` resolves the rate; `reference`
    // reads the tenant's functional currency.
    source: RateSource,
    reference: ReferenceRepo,
    // D2 (FX): reads the OPERATION's locked rate (the referenced posted entry's
    // `rate_snapshot_ref`) so the threshold is valued at the operation's own rate,
    // not a fresh gate-time rate.
    journal: JournalRepo,
}

impl ApprovalService {
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        executor: Arc<dyn ApprovalExecutor>,
        metrics: Arc<dyn LedgerMetricsPort>,
        fx_config: FxConfig,
    ) -> Self {
        let repo = ApprovalRepo::new(db.clone());
        let source = RateSource::new(FxRepo::new(db.clone()), fx_config);
        let reference = ReferenceRepo::new(db.clone());
        let journal = JournalRepo::new(db.clone());
        Self {
            db,
            repo,
            executor,
            metrics,
            source,
            reference,
            journal,
        }
    }

    /// Create (or idempotently return) a `PENDING` approval for an over-threshold
    /// mutation. A retry with the same `(tenant, kind, business_key)` returns the
    /// existing active record rather than a duplicate (DC13).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    #[allow(clippy::too_many_arguments)] // a pending record carries several snapshot fields
    pub async fn create_pending(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: ApprovalIntent,
        reason_code: String,
        threshold_snapshot: serde_json::Value,
        amount_usd_eq_minor: Option<i64>,
        ttl_seconds: i64,
    ) -> Result<Uuid, DomainError> {
        let tenant = ctx.subject_tenant_id();
        let kind = intent.kind();
        let business_key = intent.business_key();

        let now = OffsetDateTime::now_utc();
        if let Some(existing) = self
            .repo
            .read_active(scope, tenant, kind.as_str(), &business_key, now)
            .await?
        {
            return Ok(existing.approval_id);
        }

        let approval_id = Uuid::now_v7();
        let prepared_at = now;
        let expires_at = prepared_at + Duration::seconds(ttl_seconds);
        let intent_json = serde_json::to_value(&intent)
            .map_err(|e| DomainError::Internal(format!("serialize approval intent: {e}")))?;
        let row = NewPendingApproval {
            approval_id,
            tenant,
            kind: kind.as_str().to_owned(),
            business_key: business_key.clone(),
            intent: intent_json,
            amount_usd_eq_minor,
            threshold_snapshot,
            reason_code,
            prepared_by: ctx.subject_id(),
            prepared_at,
            correlation_id: Uuid::now_v7(),
            expires_at,
        };
        let scope_owned = scope.clone();
        let created = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let row = row.clone();
                let scope = scope_owned.clone();
                Box::pin(async move {
                    // Lazy expiry pass (DC13/DC12): flip this tenant's lapsed
                    // PENDING/NEEDS_REWORK rows to EXPIRED first, so an abandoned
                    // approval past its TTL no longer occupies the active-uniqueness
                    // slot and the insert below can claim it. This is the per-tenant
                    // lazy complement to the cross-tenant TTL sweep job
                    // (`expire_due_all`, wired in `module.rs`) — DC12 ships BOTH, so
                    // a slot is freed on the next prepare even between sweep ticks.
                    ApprovalRepo::expire_due(txn, &scope, tenant, now)
                        .await
                        .map_err(repo_to_db)?;
                    ApprovalRepo::insert_pending(txn, &scope, row)
                        .await
                        .map_err(repo_to_db)?;
                    Ok::<(), DbError>(())
                })
            })
            .await;
        if let Err(e) = created {
            // ONLY the DC13 active-uniqueness race is recoverable: a concurrent
            // preparer that both passed the read_active check and won the partial-
            // unique index, leaving this caller the loser (23505). Any OTHER error
            // (connection drop, CHECK violation, …) is a real failure — surface it,
            // never mask it behind a re-read. Gate on the unique-violation
            // discriminator; on a confirmed dup, return the winner idempotently.
            if as_db_err(&e).is_some_and(is_unique_violation)
                && let Some(existing) = self
                    .repo
                    .read_active(
                        scope,
                        tenant,
                        kind.as_str(),
                        &business_key,
                        OffsetDateTime::now_utc(),
                    )
                    .await?
            {
                return Ok(existing.approval_id);
            }
            return Err(DomainError::Internal(format!(
                "create pending approval: {e}"
            )));
        }
        self.metrics.dual_control_pending(kind.as_str());
        Ok(approval_id)
    }

    /// The retrofit gate (Group E): resolve the tenant's effective policy and
    /// decide whether `facts` crosses the threshold. Over threshold → create a
    /// `PENDING` approval and return its id (the handler returns
    /// `409 DUAL_CONTROL_REQUIRED`); at/under threshold → `None` (the handler
    /// proceeds single-actor, unchanged).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn gate(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: ApprovalIntent,
        facts: OperationFacts,
        reason_code: String,
    ) -> Result<Option<Uuid>, DomainError> {
        let versions = self
            .repo
            .read_policy_versions(scope, ctx.subject_tenant_id())
            .await?;
        let now = OffsetDateTime::now_utc();
        let policy = resolve_policy(&versions, now);
        // DC10 / FX: value the comparand in the tenant's FUNCTIONAL (reporting)
        // currency before the threshold compare — the threshold is denominated in
        // functional minor. A single-currency tenant (or same-currency op) compares
        // as-is; a cross-currency op translates at the OPERATION's own locked rate
        // (design D2 — deterministic + anti-circumvention), falling back to the
        // gate-time rate only when the operation carries no referenced snapshot.
        let txn_ccy = intent.transaction_currency().map(str::to_owned);
        let locked_rate_micro = self
            .operation_locked_rate(ctx.subject_tenant_id(), &intent)
            .await?;
        let facts = self
            .to_functional_facts(
                scope,
                ctx.subject_tenant_id(),
                facts,
                txn_ccy.as_deref(),
                now,
                locked_rate_micro,
            )
            .await?;
        if !requires_dual_control(&facts, policy, to_naive_date(now)) {
            return Ok(None);
        }
        let snapshot = threshold_snapshot(&policy, now);
        let id = self
            .create_pending(
                ctx,
                scope,
                intent,
                reason_code,
                snapshot,
                facts.amount_usd_eq_minor,
                policy.pending_ttl_seconds,
            )
            .await?;
        Ok(Some(id))
    }

    /// The OPERATION's own locked FX `rate_micro` for the D2 threshold (design D2:
    /// value at the operation's snapshot, not a fresh gate-time rate). Maps the intent
    /// to its referenced posted entry — refund -> the payment's `PAYMENT_SETTLE`,
    /// credit / debit note -> the invoice's `INVOICE_POST` — and reads that entry's
    /// locked rate. `None` (⇒ gate-time fallback) for a single-currency operation, an
    /// absent reference, or an intent with no rate-bearing reference (reverse /
    /// material-backdating carry no currency — a separate documented residual).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    async fn operation_locked_rate(
        &self,
        tenant: Uuid,
        intent: &ApprovalIntent,
    ) -> Result<Option<i64>, DomainError> {
        let (business_id, doc_type): (&str, SourceDocType) = match intent {
            ApprovalIntent::Refund(i) => (i.payment_id.as_str(), SourceDocType::PaymentSettle),
            ApprovalIntent::RefundWithCreditNote(i) => {
                (i.refund.payment_id.as_str(), SourceDocType::PaymentSettle)
            }
            ApprovalIntent::CreditNote(i) => {
                (i.origin_invoice_id.as_str(), SourceDocType::InvoicePost)
            }
            ApprovalIntent::DebitNote(i) => {
                (i.origin_invoice_id.as_str(), SourceDocType::InvoicePost)
            }
            _ => return Ok(None),
        };
        // Internal valuation read: a plain tenant scope (the entry is tenant-secured).
        let scope = AccessScope::for_tenant(tenant);
        self.journal
            .locked_rate_micro_for(&scope, tenant, business_id, doc_type.as_str())
            .await
            .map_err(|e| DomainError::Internal(format!("operation locked-rate lookup: {e}")))
    }

    /// Translate the comparand (`facts.amount_usd_eq_minor`, in `txn_currency`) into
    /// the tenant's FUNCTIONAL (reporting) currency for the DC10 threshold compare.
    /// Returns `facts` unchanged when there is no amount comparand, the caller did
    /// not tag a transaction currency (a non-amount kind, or `Reverse` /
    /// `RecognitionScheduleChange` whose currency is gate-time-derived), the tenant
    /// is single-currency (no functional configured), or the operation currency
    /// already IS the functional currency. Otherwise resolves the current rate and
    /// translates the amount.
    ///
    /// # Errors
    /// [`DomainError::FxRateUnavailable`] / [`DomainError::FxRateStaleNotAllowed`]
    /// when a cross-currency op has no usable rate (the post would fail the same
    /// way — the gate never silently mis-values the threshold); other
    /// [`DomainError`] on a storage / translate fault.
    async fn to_functional_facts(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        facts: OperationFacts,
        txn_currency: Option<&str>,
        now: OffsetDateTime,
        locked_rate_micro: Option<i64>,
    ) -> Result<OperationFacts, DomainError> {
        let (Some(amount), Some(txn_ccy)) = (facts.amount_usd_eq_minor, txn_currency) else {
            return Ok(facts);
        };
        let Some(functional_ccy) = self
            .reference
            .functional_currency(scope, tenant)
            .await
            .map_err(|e| {
                DomainError::Internal(format!("dual-control functional-currency lookup: {e}"))
            })?
        else {
            // Single-currency tenant: the threshold currency IS the operation currency.
            return Ok(facts);
        };
        if functional_ccy == txn_ccy {
            return Ok(facts);
        }
        // D2: prefer the operation's OWN locked rate; fall back to the gate-time rate
        // only when the operation carries no referenced snapshot (the documented
        // residual). A missing/stale rate on the fallback fails here exactly as the
        // post would.
        let rate_micro = if let Some(locked) = locked_rate_micro {
            locked
        } else {
            self.source
                .resolve(scope, tenant, txn_ccy, &functional_ccy, now)
                .await?
                .rate_micro
        };
        let functional_minor = translate_amount(amount, rate_micro)
            .map_err(|e| DomainError::Internal(format!("dual-control FX translate: {e}")))?;
        let mut facts = facts;
        facts.amount_usd_eq_minor = Some(functional_minor);
        Ok(facts)
    }

    /// Read the tenant's effective dual-control policy *version* at `now` for the
    /// read surface (`GET /dual-control-policy`): the row in force (greatest
    /// `effective_from <= now`, highest `version` on a tie), or `None` when the
    /// tenant has set no row and the ratified [`DualControlPolicy::DEFAULT`]
    /// applies. Tenant-scoped — `scope` is the SQL-level BOLA filter, so a tenant
    /// outside the caller's authorized subtree reads as no rows ⇒ `None` ⇒ the
    /// platform defaults (the thresholds are public constants, so this leaks
    /// neither row existence nor a configured value).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn read_effective_policy(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        now: OffsetDateTime,
    ) -> Result<Option<PolicyVersion>, DomainError> {
        let versions = self.repo.read_policy_versions(scope, tenant).await?;
        Ok(effective_version(&versions, now))
    }

    /// Write a new effective-dated dual-control threshold policy version for the
    /// caller's tenant (DC8). Validates the D2/A6/TTL ranges first (out-of-range →
    /// [`DomainError::DualControlPolicyOutOfRange`], never clamped — DC9/DC11), then
    /// appends a `(tenant, version = max + 1)` row in one serializable txn. The
    /// resolver picks the latest `effective_from` (highest `version` on a tie), so a
    /// later write supersedes without mutating history. Returns the new `version`.
    ///
    /// # Errors
    /// [`DomainError::DualControlPolicyOutOfRange`] on an out-of-range threshold;
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn set_policy(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        d2_threshold_minor: i64,
        a6_backdating_biz_days: i32,
        pending_ttl_seconds: i64,
        effective_from: OffsetDateTime,
    ) -> Result<i64, DomainError> {
        validate_config(
            d2_threshold_minor,
            a6_backdating_biz_days,
            pending_ttl_seconds,
        )
        .map_err(policy_config_to_domain)?;
        let tenant = ctx.subject_tenant_id();
        let created_at_utc = OffsetDateTime::now_utc();
        let scope_c = scope.clone();
        let version = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let scope = scope_c.clone();
                Box::pin(async move {
                    let next = ApprovalRepo::max_policy_version(txn, &scope, tenant)
                        .await
                        .map_err(repo_to_db)?
                        .map_or(0, |v| v + 1);
                    ApprovalRepo::insert_policy_row(
                        txn,
                        &scope,
                        NewPolicyVersion {
                            tenant,
                            version: next,
                            effective_from,
                            d2_threshold_minor,
                            a6_backdating_biz_days,
                            pending_ttl_seconds,
                            created_at_utc,
                        },
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Ok::<i64, DbError>(next)
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("set dual-control policy txn: {e}")))?;
        Ok(version)
    }

    /// Approve an approval: latch `PENDING → APPROVING` (so a concurrent
    /// reject/cancel/request-changes can no longer win), execute the stored
    /// mutation (idempotent), then mark `APPROVING → APPROVED` + write the decision
    /// audit. A row already `APPROVING` (a crash-recovery retry) skips the latch and
    /// re-executes idempotently. A mutation failure reverts the latch to `PENDING`.
    ///
    /// # Errors
    /// [`DomainError::ApprovalNotFound`] / [`DomainError::ApprovalNotActionable`]
    /// (wrong state, expired, or lost the latch race) / [`DomainError::SelfApprovalForbidden`]
    /// (`approver == preparer`); the mutation's own rejection propagates unchanged.
    pub async fn approve(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let approver = ctx.subject_id();
        let row = self.load_for_approve(scope, tenant, approval_id).await?;
        if approver == row.prepared_by {
            self.metrics.dual_control_self_approval_denied(&row.kind);
            return Err(DomainError::SelfApprovalForbidden(format!(
                "approver must differ from preparer for approval {approval_id}"
            )));
        }
        let intent: ApprovalIntent = serde_json::from_value(row.intent.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize approval intent: {e}")))?;

        // (0) Latch PENDING → APPROVING in its own txn. Once latched, the decision
        //     verbs (reject/cancel/request-changes — all keyed on PENDING) match no
        //     row and fail, so the mutation about to execute can never be retro-
        //     actively rejected (H2). A row already APPROVING is a crash-recovery
        //     retry: skip the latch and re-execute idempotently.
        if parse_state(&row.state)? == ApprovalState::Pending
            && !self
                .bare_transition(
                    tenant,
                    scope,
                    approval_id,
                    ApprovalState::Pending,
                    ApprovalState::Approving,
                )
                .await?
        {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} was not in PENDING (concurrent decision)"
            )));
        }

        // (1) idempotent mutation in its own transaction.
        if let Err(e) = self.executor.execute(ctx, scope, &intent).await {
            // The mutation's txn rolled back (nothing committed), so the approval
            // is safe to return to PENDING — actionable again (re-approve / reject /
            // rework). Best-effort: a failed revert leaves the row APPROVING, which
            // a later approve recovers idempotently. The original error propagates.
            if let Err(revert) = self
                .bare_transition(
                    tenant,
                    scope,
                    approval_id,
                    ApprovalState::Approving,
                    ApprovalState::Pending,
                )
                .await
            {
                tracing::error!(
                    error = %revert,
                    %approval_id,
                    "bss-ledger: failed to revert APPROVING→PENDING after an executor error"
                );
            }
            return Err(e);
        }

        // (2) mark APPROVED + decision audit (APPROVING → APPROVED) in one txn.
        let body = decision_audit(
            "approved",
            &row.kind,
            &row.business_key,
            row.prepared_by,
            approver,
            None,
        );
        self.commit_transition(
            tenant,
            scope,
            approval_id,
            ApprovalState::Approving,
            ApprovalState::Approved,
            approver,
            row.revision,
            body,
        )
        .await?;
        self.metrics.dual_control_decided(&row.kind, "approved");
        Ok(())
    }

    /// Reject a `PENDING` approval with a mandatory reason. The mutation never
    /// runs.
    ///
    /// # Errors
    /// As [`Self::approve`] (minus the executor), with the reason recorded.
    pub async fn reject(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
        reason: String,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let decider = ctx.subject_id();
        let row = self.load_pending(scope, tenant, approval_id).await?;
        if decider == row.prepared_by {
            self.metrics.dual_control_self_approval_denied(&row.kind);
            return Err(DomainError::SelfApprovalForbidden(format!(
                "rejecter must differ from preparer for approval {approval_id}"
            )));
        }
        let body = decision_audit(
            "rejected",
            &row.kind,
            &row.business_key,
            row.prepared_by,
            decider,
            Some(&reason),
        );
        self.commit_transition(
            tenant,
            scope,
            approval_id,
            ApprovalState::Pending,
            ApprovalState::Rejected,
            decider,
            row.revision,
            body,
        )
        .await?;
        self.metrics.dual_control_decided(&row.kind, "rejected");
        Ok(())
    }

    /// Cancel an active (`PENDING`/`NEEDS_REWORK`) approval — only the preparer
    /// may withdraw their own request.
    ///
    /// # Errors
    /// [`DomainError::ApprovalNotFound`] / [`DomainError::ApprovalNotActionable`]
    /// (terminal, or caller is not the preparer / lost race).
    pub async fn cancel(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let caller = ctx.subject_id();
        let row = self
            .repo
            .read(scope, tenant, approval_id)
            .await?
            .ok_or_else(|| DomainError::ApprovalNotFound(format!("approval {approval_id}")))?;
        let state = parse_state(&row.state)?;
        if !state.is_active() {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} is {} (terminal)",
                row.state
            )));
        }
        if caller != row.prepared_by {
            return Err(DomainError::ApprovalNotActionable(format!(
                "only the preparer may cancel approval {approval_id}"
            )));
        }
        let body = decision_audit(
            "cancelled",
            &row.kind,
            &row.business_key,
            row.prepared_by,
            caller,
            None,
        );
        self.commit_transition(
            tenant,
            scope,
            approval_id,
            state,
            ApprovalState::Cancelled,
            caller,
            row.revision,
            body,
        )
        .await?;
        self.metrics.dual_control_decided(&row.kind, "cancelled");
        Ok(())
    }

    /// Return a `PENDING` approval to the preparer for rework, with a mandatory
    /// reason. The mutation never runs; the preparer edits and `resubmit`s.
    ///
    /// # Errors
    /// As [`Self::reject`].
    pub async fn request_changes(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
        reason: String,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let decider = ctx.subject_id();
        let row = self.load_pending(scope, tenant, approval_id).await?;
        if decider == row.prepared_by {
            self.metrics.dual_control_self_approval_denied(&row.kind);
            return Err(DomainError::SelfApprovalForbidden(format!(
                "changer must differ from preparer for approval {approval_id}"
            )));
        }
        let body = decision_audit(
            "needs_rework",
            &row.kind,
            &row.business_key,
            row.prepared_by,
            decider,
            Some(&reason),
        );
        self.commit_transition(
            tenant,
            scope,
            approval_id,
            ApprovalState::Pending,
            ApprovalState::NeedsRework,
            decider,
            row.revision,
            body,
        )
        .await?;
        self.metrics.dual_control_decided(&row.kind, "needs_rework");
        Ok(())
    }

    /// Resubmit a `NEEDS_REWORK` approval back to `PENDING` with the preparer's
    /// edited intent, bumping `revision`. The kind cannot change. Re-evaluates the
    /// threshold on the edited intent and re-snapshots the policy in force (DC17);
    /// an approval, once required, is never dropped by shrinking the amount —
    /// resubmit always returns to `PENDING`, never auto-applies.
    ///
    /// # Errors
    /// [`DomainError::ApprovalNotFound`] / [`DomainError::ApprovalNotActionable`]
    /// (not awaiting rework, not the preparer, kind changed, or lost race).
    pub async fn resubmit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
        new_intent: ApprovalIntent,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let caller = ctx.subject_id();
        let row = self
            .repo
            .read(scope, tenant, approval_id)
            .await?
            .ok_or_else(|| DomainError::ApprovalNotFound(format!("approval {approval_id}")))?;
        if parse_state(&row.state)? != ApprovalState::NeedsRework {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} is {}, expected NEEDS_REWORK",
                row.state
            )));
        }
        if caller != row.prepared_by {
            return Err(DomainError::ApprovalNotActionable(format!(
                "only the preparer may resubmit approval {approval_id}"
            )));
        }
        if new_intent.kind().as_str() != row.kind {
            return Err(DomainError::ApprovalNotActionable(
                "cannot change the approval kind on resubmit".to_owned(),
            ));
        }
        // DC #1: pin the target identity. Re-hydrate the stored intent
        // and require the resubmitted one to address the SAME target — only the
        // scalar amount may be edited (DC17). Without this a preparer could, after a
        // request-changes, swap the recipient (payer_tenant_id / entry_id /
        // payment_id) under the still-frozen business_key, and the approver would
        // book the credit / reversal / chargeback to the swapped party: the executor
        // replays the stored body and `ApprovalDto` never surfaces the recipient.
        let original_intent: ApprovalIntent = serde_json::from_value(row.intent.clone())
            .map_err(|e| DomainError::Internal(format!("deserialize approval intent: {e}")))?;
        if !new_intent.same_target(&original_intent) {
            return Err(DomainError::ApprovalNotActionable(
                "resubmit cannot change the approval target; only the amount may be edited"
                    .to_owned(),
            ));
        }
        // DC17: re-evaluate the threshold against the EDITED intent and re-snapshot
        // the policy in force NOW (not a stub). The recorded `threshold_snapshot`
        // must reflect the policy that applied at resubmit time; the approval is
        // never silently dropped (it always returns to PENDING, so an approver is
        // still required even if the edited amount is now below threshold).
        let versions = self.repo.read_policy_versions(scope, tenant).await?;
        let resolved_at = OffsetDateTime::now_utc();
        let policy = resolve_policy(&versions, resolved_at);
        let new_threshold_snapshot = threshold_snapshot(&policy, resolved_at);
        let new_amount_usd_eq_minor = new_intent.amount_minor();
        let new_revision = row.revision + 1;
        let intent_json = serde_json::to_value(&new_intent)
            .map_err(|e| DomainError::Internal(format!("serialize approval intent: {e}")))?;
        let body = decision_audit(
            "resubmitted",
            &row.kind,
            &row.business_key,
            row.prepared_by,
            caller,
            None,
        );
        let scope_c = scope.clone();
        let now = OffsetDateTime::now_utc();
        let comment_id = Uuid::now_v7();
        let applied = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let scope = scope_c.clone();
                let intent_json = intent_json.clone();
                let snapshot = new_threshold_snapshot.clone();
                let body = body.clone();
                Box::pin(async move {
                    let rows = ApprovalRepo::resubmit(
                        txn,
                        &scope,
                        tenant,
                        approval_id,
                        intent_json,
                        snapshot,
                        new_amount_usd_eq_minor,
                        new_revision,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    if rows == 0 {
                        return Ok::<bool, DbError>(false);
                    }
                    ApprovalRepo::append_comment(
                        txn,
                        &scope,
                        comment_id,
                        approval_id,
                        tenant,
                        new_revision,
                        caller,
                        body,
                        now,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Ok(true)
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("approval resubmit txn: {e}")))?;
        if !applied {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} was not in NEEDS_REWORK (concurrent decision)"
            )));
        }
        self.metrics.dual_control_decided(&row.kind, "resubmitted");
        Ok(())
    }

    /// Append a free comment / question to an approval's thread (no state change).
    /// The author is the caller; authz (preparer or `entry_approve.v1`) is gated
    /// at the REST layer.
    ///
    /// # Errors
    /// [`DomainError::ApprovalNotFound`] / [`DomainError::Internal`].
    pub async fn add_comment(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
        body_text: String,
    ) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let author = ctx.subject_id();
        let row = self
            .repo
            .read(scope, tenant, approval_id)
            .await?
            .ok_or_else(|| DomainError::ApprovalNotFound(format!("approval {approval_id}")))?;
        let revision = row.revision;
        let scope_c = scope.clone();
        let now = OffsetDateTime::now_utc();
        let comment_id = Uuid::now_v7();
        self.db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let scope = scope_c.clone();
                let body = body_text.clone();
                Box::pin(async move {
                    ApprovalRepo::append_comment(
                        txn,
                        &scope,
                        comment_id,
                        approval_id,
                        tenant,
                        revision,
                        author,
                        body,
                        now,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Ok::<(), DbError>(())
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("add comment txn: {e}")))?;
        Ok(())
    }

    /// List the tenant's approval queue (newest-first), optionally filtered.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn list(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        state: Option<&str>,
        kind: Option<&str>,
    ) -> Result<Vec<crate::infra::storage::entity::dual_control_approval::Model>, DomainError> {
        self.repo
            .list(scope, ctx.subject_tenant_id(), state, kind)
            .await
    }

    /// Read a single approval.
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
    ) -> Result<Option<crate::infra::storage::entity::dual_control_approval::Model>, DomainError>
    {
        self.repo
            .read(scope, ctx.subject_tenant_id(), approval_id)
            .await
    }

    /// Read an approval's comment thread (oldest-first).
    ///
    /// # Errors
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn thread(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        approval_id: Uuid,
    ) -> Result<Vec<crate::infra::storage::entity::dual_control_comment::Model>, DomainError> {
        self.repo
            .read_thread(scope, ctx.subject_tenant_id(), approval_id)
            .await
    }

    /// Read + validate that an approval is approvable: `PENDING` (must be
    /// unexpired) or `APPROVING` (a crash-recovery retry — the decision was already
    /// latched, so expiry no longer gates it). Any other state is not actionable.
    async fn load_for_approve(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
    ) -> Result<crate::infra::storage::entity::dual_control_approval::Model, DomainError> {
        let row = self
            .repo
            .read(scope, tenant, approval_id)
            .await?
            .ok_or_else(|| DomainError::ApprovalNotFound(format!("approval {approval_id}")))?;
        match parse_state(&row.state)? {
            ApprovalState::Pending => {
                if row.expires_at <= OffsetDateTime::now_utc() {
                    return Err(DomainError::ApprovalNotActionable(format!(
                        "approval {approval_id} expired at {}",
                        row.expires_at
                    )));
                }
            }
            // Recovery: a prior approve latched APPROVING then crashed before the
            // mark; re-execute idempotently and complete it. Expiry no longer gates
            // a decision already taken.
            ApprovalState::Approving => {}
            _ => {
                return Err(DomainError::ApprovalNotActionable(format!(
                    "approval {approval_id} is {}, expected PENDING",
                    row.state
                )));
            }
        }
        Ok(row)
    }

    /// Apply a bare state transition (NO audit comment) in its own serializable
    /// txn; returns `true` iff the row was in `expected`. The H2 `APPROVING` latch
    /// and its revert are control-flow latches, not audited decisions, so they skip
    /// the audit append `commit_transition` writes; `approved_by`/`decided_at` stay
    /// `NULL` (stamped only by the final `APPROVING → APPROVED` `commit_transition`).
    async fn bare_transition(
        &self,
        tenant: Uuid,
        scope: &AccessScope,
        approval_id: Uuid,
        expected: ApprovalState,
        new_state: ApprovalState,
    ) -> Result<bool, DomainError> {
        let scope = scope.clone();
        let expected_s = expected.as_str();
        let new_s = new_state.as_str();
        let applied = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let scope = scope.clone();
                Box::pin(async move {
                    let rows = ApprovalRepo::transition(
                        txn,
                        &scope,
                        tenant,
                        approval_id,
                        expected_s,
                        new_s,
                        None,
                        None,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Ok::<bool, DbError>(rows > 0)
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("approval latch txn: {e}")))?;
        Ok(applied)
    }

    /// Read + validate that an approval is `PENDING` and not expired.
    async fn load_pending(
        &self,
        scope: &AccessScope,
        tenant: Uuid,
        approval_id: Uuid,
    ) -> Result<crate::infra::storage::entity::dual_control_approval::Model, DomainError> {
        let row = self
            .repo
            .read(scope, tenant, approval_id)
            .await?
            .ok_or_else(|| DomainError::ApprovalNotFound(format!("approval {approval_id}")))?;
        let state = parse_state(&row.state)?;
        if state != ApprovalState::Pending {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} is {}, expected PENDING",
                row.state
            )));
        }
        if row.expires_at <= OffsetDateTime::now_utc() {
            return Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} expired at {}",
                row.expires_at
            )));
        }
        Ok(row)
    }

    /// Apply a state transition + append the decision audit in one serializable
    /// transaction. The optimistic `expected_state` filter is the in-txn race
    /// backstop: `0` rows means another decision won (or the row moved), which
    /// surfaces as [`DomainError::ApprovalNotActionable`].
    #[allow(clippy::too_many_arguments)] // a decision is intrinsically wide; a struct adds churn
    async fn commit_transition(
        &self,
        tenant: Uuid,
        scope: &AccessScope,
        approval_id: Uuid,
        expected: ApprovalState,
        new_state: ApprovalState,
        decider: Uuid,
        revision: i32,
        audit_body: String,
    ) -> Result<(), DomainError> {
        let scope = scope.clone();
        let expected_s = expected.as_str();
        let new_s = new_state.as_str();
        // `approved_by` is stamped ONLY for the actual APPROVED transition. A
        // reject / request-changes / CANCEL is not an approval — stamping the actor
        // there would, for a preparer self-cancel, set `approved_by == prepared_by`
        // and trip the `approved_by <> prepared_by` CHECK (surfacing as a 500). The
        // audit comment below records who acted regardless of the lifecycle state.
        let approved_by = (new_state == ApprovalState::Approved).then_some(decider);
        let now = OffsetDateTime::now_utc();
        let comment_id = Uuid::now_v7();
        let applied = self
            .db
            .db()
            .transaction_with_retry(TxConfig::serializable(), as_db_err, move |txn| {
                let scope = scope.clone();
                let body = audit_body.clone();
                Box::pin(async move {
                    let rows = ApprovalRepo::transition(
                        txn,
                        &scope,
                        tenant,
                        approval_id,
                        expected_s,
                        new_s,
                        approved_by,
                        Some(now),
                    )
                    .await
                    .map_err(repo_to_db)?;
                    if rows == 0 {
                        return Ok::<bool, DbError>(false);
                    }
                    ApprovalRepo::append_comment(
                        txn,
                        &scope,
                        comment_id,
                        approval_id,
                        tenant,
                        revision,
                        decider,
                        body,
                        now,
                    )
                    .await
                    .map_err(repo_to_db)?;
                    Ok(true)
                })
            })
            .await
            .map_err(|e| DomainError::Internal(format!("approval decision txn: {e}")))?;
        if applied {
            Ok(())
        } else {
            Err(DomainError::ApprovalNotActionable(format!(
                "approval {approval_id} was not in {expected_s} (concurrent decision or stale state)"
            )))
        }
    }
}

/// Parse a stored state token, mapping an unknown literal to an internal fault.
fn parse_state(s: &str) -> Result<ApprovalState, DomainError> {
    ApprovalState::parse(s)
        .ok_or_else(|| DomainError::Internal(format!("unknown approval state {s:?}")))
}

/// The threshold-snapshot recorded on a pending approval: which policy values
/// applied + when resolved (audit trail; DC8/DC17). Built identically by `gate`
/// (on create) and `resubmit` (on re-evaluation against the edited intent), so the
/// recorded snapshot is never a stub.
fn threshold_snapshot(policy: &DualControlPolicy, now: OffsetDateTime) -> serde_json::Value {
    serde_json::json!({
        "d2_threshold_minor": policy.d2_threshold_minor,
        "a6_backdating_biz_days": policy.a6_backdating_biz_days,
        "pending_ttl_seconds": policy.pending_ttl_seconds,
        "resolved_at": format_rfc3339(now),
    })
}

/// Map a pure policy-config range rejection (DC9/DC11) to the domain error: an
/// out-of-range D2/A6/TTL is `DualControlPolicyOutOfRange` (→ 409, no clamp).
fn policy_config_to_domain(e: PolicyConfigError) -> DomainError {
    let detail = match e {
        PolicyConfigError::D2OutOfRange(v) => {
            format!("d2_threshold_minor {v} out of range [10000..100000000]")
        }
        PolicyConfigError::A6OutOfRange(v) => {
            format!("a6_backdating_biz_days {v} out of range [1..30]")
        }
        PolicyConfigError::TtlNotPositive(v) => format!("pending_ttl_seconds {v} must be > 0"),
    };
    DomainError::DualControlPolicyOutOfRange(detail)
}

/// The structured decision-audit body recorded on the append-only comment thread
/// (the Slice-2 stand-in for `secured_audit_record`, DC7). `event_type` mirrors
/// the design's `exception-resolution` audit class.
fn decision_audit(
    decision: &str,
    kind: &str,
    business_key: &str,
    prepared_by: Uuid,
    decided_by: Uuid,
    reason: Option<&str>,
) -> String {
    serde_json::json!({
        "event_type": "exception-resolution",
        "decision": decision,
        "kind": kind,
        "business_key": business_key,
        "prepared_by": prepared_by,
        "decided_by": decided_by,
        "reason": reason,
    })
    .to_string()
}

/// Retry-extractor: a wrapped `DbErr` is recognised as retryable serialization
/// contention (mirrors `PostingService::as_db_err`).
fn as_db_err(e: &DbError) -> Option<&DbErr> {
    match e {
        DbError::Sea(db_err) => Some(db_err),
        _ => None,
    }
}

/// Encode a repo failure as a non-retryable `DbError` so the decision txn rolls
/// back and surfaces as an internal fault (business outcomes are carried by the
/// `bool`/`rows` path, not by error).
#[allow(clippy::needless_pass_by_value)] // used as a `map_err` fn pointer (FnOnce(RepoError))
fn repo_to_db(e: RepoError) -> DbError {
    DbError::Sea(DbErr::Custom(format!("approval repo: {e:?}")))
}
