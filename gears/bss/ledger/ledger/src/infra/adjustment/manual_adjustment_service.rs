//! `ManualAdjustmentHandler` — the Slice-3 Phase-3 governed manual-adjustment
//! orchestrator (design §4.6, Group 4). It posts a governed manual adjustment's
//! balanced entry through the invariant [`PostingService`] after clearing the pure
//! [`govern`] gate, and publishes `billing.ledger.manual_adjustment.posted` in the
//! SAME post txn (via a [`PostSidecar`]). It is the ledger's governed escape hatch
//! for corrections the typed flows (invoice / settle / allocate / S3 notes / S4
//! recognition) do not cover — rounding residue, suspense / cash-clearing clean-up.
//!
//! **Flow.**
//! 1. **govern** ([`govern`], pure, out-of-txn): a [`ManualAdjustmentReject`]
//!    short-circuits the post. A generic [`ManualAdjustmentReject::NotAllowed`]
//!    becomes [`DomainError::ManualAdjustmentNotAllowed`] (a 400, no books effect);
//!    a [`ManualAdjustmentReject::AttemptedWriteOff`] additionally fires a
//!    [`SecuredAuditSink`] capture + the `AttemptedWriteOff` page out-of-band
//!    (Group 4 / §6 A4) before the same 400 — a disguised bad-debt is a
//!    deliberate-misuse signal, not a benign typo.
//! 2. **payer gate**: an `AR` / `UNALLOCATED` leg posts against a payer-scoped
//!    balance, so `payer_tenant_id` MUST be present (the projector grains AR /
//!    UNALLOCATED per payer); otherwise the adjustment is a payer-less internal
//!    move attributed to the tenant itself.
//! 3. **assemble + post**: resolve each leg's chart account + currency scale, build
//!    the balanced engine entry (`source_doc_type = MANUAL_ADJUSTMENT`,
//!    `source_business_id = adjustment_id`), and post it with the
//!    [`ManualAdjustmentPostSidecar`] (publishes the event in the post txn).
//!
//! **Idempotency** is the engine's `(tenant, MANUAL_ADJUSTMENT, adjustment_id)`
//! claim: the entry's `source_doc_type = ManualAdjustment` + `source_business_id =
//! adjustment_id` make [`PostingService::post`]'s `Fresh` claim the at-most-once
//! gate (a replay returns before the sidecar — byte-identical to the notes / refund
//! handlers).
//!
//! **Dual-control (Group 5 / Phase 3).** `SoD` (`preparer ≠ approver`) + the lifecycle
//! are the `ApprovalService`'s concern; this handler holds an OPTIONAL
//! [`ApprovalService`](crate::infra::approval::service::ApprovalService) seam
//! ([`ManualAdjustmentHandler::with_approval`]) and, when wired, gates a governed
//! manual adjustment whose gross (`Σ DR`) crosses the tenant's D2 threshold to the
//! preparer→approver queue ([`DomainError::DualControlRequired`]) BEFORE the post
//! (mirroring the refund gate). The approved replay re-enters through
//! [`ManualAdjustmentHandler::post_manual_adjustment_approved`] (gate-skipped,
//! executor-only). The un-gated handler (`None` approval) is the executor's replay
//! handler; the REST surface wires the gated one. There is also NO schedule /
//! headroom / record sidecar row
//! (unlike the credit-note sidecar): a governed manual adjustment touches no
//! `recognition_schedule` / `invoice_exposure` and writes no per-adjustment record
//! table in this MVP — its durable trail is the journal entry + the published event
//! (+ the secured-audit capture on the write-off path).
//!
//! Lives in `infra` (not `domain`): it needs repo + posting access; the
//! [`manual`](crate::domain::adjustment::manual) domain it calls stays pure (dylint
//! DE0301). Wraps the `pub` [`PostingService`] + repos directly (like
//! [`CreditNoteHandler`](super::credit_note_service::CreditNoteHandler)) so it is
//! constructible from out-of-crate integration tests.

use std::sync::Arc;

use bss_ledger_sdk::{AccountClass, MappingStatus, PostingRef, Side, SourceDocType};
use chrono::Datelike;
use sea_orm::DbErr;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::adjustment::manual::{
    ManualAdjustmentReject, ManualAdjustmentRequest, ManualLeg, govern,
};
use crate::domain::approval::ApprovalKind;
use crate::domain::approval::intent::{ApprovalIntent, ManualAdjustmentIntent};
use crate::domain::approval::policy::OperationFacts;
use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::model::{NewEntry, NewLine};
use crate::infra::approval::service::ApprovalService;
use crate::infra::audit::secured_audit_sink::{AuditEventType, SecuredAuditSink};
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::{
    AlarmCategory, AlarmSeverity, LedgerInvariantAlarm, ManualAdjustmentPosted,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::posting::chart::load_chart;
use crate::infra::posting::service::{PostSidecar, PostedFacts, PostingService};
use crate::infra::storage::repo::ReferenceRepo;
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service (mirrors the peer
/// orchestrators).
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// The ledger error code stamped on the `AttemptedWriteOff` page + the
/// secured-audit capture (the canonical 400 the handler maps the reject to).
const CODE_MANUAL_ADJUSTMENT_NOT_ALLOWED: &str = "MANUAL_ADJUSTMENT_NOT_ALLOWED";

/// Orchestrates the governed manual-adjustment domain over the foundation engine
/// (design §4.6).
pub struct ManualAdjustmentHandler {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    /// The event publisher — threaded into the posting engine AND held so the in-txn
    /// sidecar can publish `billing.ledger.manual_adjustment.posted`, and so the
    /// write-off path can raise the `AttemptedWriteOff` alarm out-of-band.
    publisher: Arc<LedgerEventPublisher>,
    /// The Slice-6 secured-audit port — the write-off path captures the actor on its
    /// OWN committed transaction (the post was rejected, so there is no post txn to
    /// ride). No-op until Slice 6 (VHP-1858) merges its real store.
    audit: Arc<dyn SecuredAuditSink>,
    /// Provider held to open the out-of-band write-off capture transaction (the
    /// reject has no post txn to ride). A cheap clone of the same provider the
    /// posting engine + repos wrap.
    db: DBProvider<DbError>,
    /// The dual-control engine (VHP-1852, Group 5 / Phase 3). `Some` ⇒ a governed
    /// manual adjustment whose gross crosses the tenant's D2 threshold is gated to the
    /// preparer→approver queue ([`DomainError::DualControlRequired`]) instead of
    /// posting inline; `None` ⇒ gating is disabled (the executor's approved replay,
    /// and the Group-4 unit tests that construct the handler without the engine).
    /// Wired in `module` (Group 6) via [`Self::with_approval`]; mirrors the
    /// [`RefundHandler`](super::refund_service::RefundHandler)'s `approval` seam.
    approval: Option<Arc<ApprovalService>>,
}

impl ManualAdjustmentHandler {
    /// Build the handler over one database provider, the event publisher (threaded
    /// into the posting engine, the sidecar's in-txn publish, and the write-off
    /// alarm), and the secured-audit sink (the write-off capture). Same `db` /
    /// `publisher` deps as the peer
    /// [`CreditNoteHandler`](super::credit_note_service::CreditNoteHandler); the
    /// `audit` sink mirrors the
    /// [`RefundHandler`](super::refund_service::RefundHandler)'s `unknown_final`
    /// disposition wiring. No metrics: a write-off is observed through the
    /// `AttemptedWriteOff` alarm, and there is no §9 manual-adjustment counter.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        audit: Arc<dyn SecuredAuditSink>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        Self {
            posting,
            reference,
            resolver,
            publisher,
            audit,
            db,
            approval: None,
        }
    }

    /// Attach the dual-control engine (Group 5 / Phase 3): a governed manual
    /// adjustment whose gross (`Σ DR`) crosses the tenant's D2 threshold is then gated
    /// to the preparer→approver queue ([`DomainError::DualControlRequired`]) rather
    /// than posting inline. The approved replay re-enters through
    /// [`Self::post_manual_adjustment_approved`], which skips the gate. Builder form
    /// (not a `new` arg) so the executor's un-gated handler and the unit tests stay
    /// source-compatible; mirrors the
    /// [`RefundHandler::with_approval`](super::refund_service::RefundHandler::with_approval).
    #[must_use]
    pub fn with_approval(mut self, approval: Arc<ApprovalService>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Post a governed manual adjustment (design §4.6). Runs the pure [`govern`]
    /// gate, the payer gate, the dual-control gate (over the D2 threshold), assembles
    /// the balanced legs, and posts them with the in-txn [`ManualAdjustmentPostSidecar`]
    /// (event publish). Idempotent on `(tenant, MANUAL_ADJUSTMENT, adjustment_id)`.
    ///
    /// # Errors
    /// [`DomainError::ManualAdjustmentNotAllowed`] for a governance reject (a generic
    /// [`ManualAdjustmentReject::NotAllowed`] OR the
    /// [`ManualAdjustmentReject::AttemptedWriteOff`] — both map to the canonical 400,
    /// the latter additionally captured + paged), or for a missing `payer_tenant_id`
    /// on an `AR` / `UNALLOCATED` leg; [`DomainError::DualControlRequired`] when the
    /// gross crosses the tenant's D2 threshold (a PENDING approval is created — the
    /// REST handler maps it to 409); [`DomainError::AccountClosed`] when a leg's class
    /// has no provisioned account; any foundation rejection or
    /// [`DomainError::Internal`] on an infra fault.
    pub async fn post_manual_adjustment(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ManualAdjustmentRequest,
    ) -> Result<PostingRef, DomainError> {
        self.post_manual_adjustment_inner(ctx, scope, req, /* gate */ true)
            .await
    }

    /// The approved-replay entry (Group 5 / Phase 3): re-drive a held manual
    /// adjustment WITHOUT the dual-control gate. Called only by the `ApprovalExecutor`
    /// after a second actor approves the PENDING approval — the threshold was already
    /// crossed at gate time, so re-checking it would re-open a second approval (an
    /// infinite loop). Idempotent on the engine's
    /// `(tenant, MANUAL_ADJUSTMENT, adjustment_id)` claim: a re-approve replays the
    /// post harmlessly (the dedup short-circuits a committed entry before the
    /// sidecar), so execute-then-mark is safe. Mirrors
    /// [`RefundHandler::post_refund_approved`](super::refund_service::RefundHandler::post_refund_approved).
    ///
    /// # Errors
    /// As [`Self::post_manual_adjustment`], minus the dual-control gate (never returns
    /// [`DomainError::DualControlRequired`]).
    pub async fn post_manual_adjustment_approved(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ManualAdjustmentRequest,
    ) -> Result<PostingRef, DomainError> {
        self.post_manual_adjustment_inner(ctx, scope, req, /* gate */ false)
            .await
    }

    /// Shared body for [`Self::post_manual_adjustment`] (gated) +
    /// [`Self::post_manual_adjustment_approved`] (the approved replay). `gate` ⇒ a
    /// governed manual adjustment over the D2 threshold routes to dual-control instead
    /// of posting. Order: govern → payer gate → dual-control gate → assemble → post.
    async fn post_manual_adjustment_inner(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: ManualAdjustmentRequest,
        gate: bool,
    ) -> Result<PostingRef, DomainError> {
        // 1. Pure governance gate (design §4.6). A Reject short-circuits the post.
        match govern(&req) {
            Ok(()) => {}
            Err(ManualAdjustmentReject::NotAllowed(d)) => {
                // A plain governance violation: the canonical 400, no books effect,
                // no alarm / capture.
                return Err(DomainError::ManualAdjustmentNotAllowed(d));
            }
            Err(ManualAdjustmentReject::AttemptedWriteOff(d)) => {
                // A disguised bad-debt write-off: capture the actor + page Revenue
                // Assurance out-of-band (the post is rejected with NO books effect),
                // then return the SAME canonical 400 as the generic reject.
                self.capture_and_page_write_off(ctx, scope, &req, &d).await;
                return Err(DomainError::ManualAdjustmentNotAllowed(d));
            }
        }

        // 2. Payer gate: an AR / UNALLOCATED leg posts against a payer-scoped balance
        //    (the projector grains AR / UNALLOCATED per payer), so payer_tenant_id
        //    MUST be present. A payer-less internal clean-up (SUSPENSE / CASH_CLEARING
        //    / GOODWILL only) is attributed to the tenant itself.
        let touches_payer_class = req.legs.iter().any(|l| {
            matches!(
                l.account_class,
                AccountClass::Ar | AccountClass::Unallocated
            )
        });
        if touches_payer_class && req.payer_tenant_id.is_none() {
            return Err(DomainError::ManualAdjustmentNotAllowed(
                "AR/UNALLOCATED leg requires payer_tenant_id".to_owned(),
            ));
        }
        let payer = req.payer_tenant_id.unwrap_or(req.tenant_id);

        // 2b. Dual-control gate (VHP-1852, Group 5 / Phase 3 / design §1.4 D2 / §4.6).
        //     A governed manual adjustment is a money-affecting governed posting, gated
        //     BEFORE the post (the journal entry does not exist at gate time — like a
        //     refund). Above the tenant's D2 threshold ⇒ a PENDING approval is created
        //     and `DualControlRequired` is returned (the REST handler maps it to 409);
        //     at/under threshold ⇒ inline, unchanged. The approved replay
        //     (`gate == false`) skips this — the threshold was already crossed at gate
        //     time. `assemble_post` re-derives the gross independently below (both =
        //     Σ DR), which is fine.
        if gate && let Some(approval) = &self.approval {
            // Gross = Σ DR (== Σ CR; govern balanced the legs). `i128` accumulate then
            // i64 cast (govern rejected an out-of-i64 set) — the D2 comparand.
            let gross_i128: i128 = req
                .legs
                .iter()
                .filter(|l| l.side == Side::Debit)
                .map(|l| i128::from(l.amount_minor))
                .sum();
            let gross = i64::try_from(gross_i128).map_err(|_| {
                DomainError::Internal("manual adjustment gross overflows i64".to_owned())
            })?;
            let intent = ApprovalIntent::ManualAdjustment(ManualAdjustmentIntent::from(&req));
            let facts = OperationFacts {
                kind: ApprovalKind::ManualAdjustment,
                // FX-SIMPLIFICATION (DC10 / FX = Slice 5): the comparand is the
                // adjustment's TRANSACTION-currency minor, not a USD-eq translation.
                // The ledger is single-currency until the FX slice lands (transaction
                // == functional currency), so the D2 compare is currency-correct today;
                // when FX lands this MUST source the operation's rate snapshot. Mirrors
                // the refund gate's comment.
                amount_usd_eq_minor: Some(gross),
                effective_at: None,
                has_outstanding_balance: false,
            };
            if let Some(approval_id) = approval
                .gate(ctx, scope, intent, facts, "manual_adjustment".to_owned())
                .await?
            {
                return Err(DomainError::DualControlRequired(format!(
                    "manual adjustment requires dual-control approval: {approval_id}"
                )));
            }
        }

        // 3. Resolve chart accounts + scales, assemble the engine entry + lines, and
        //    compute the gross amount (Σ DR == Σ CR; govern guaranteed the balance).
        let (entry, lines, amount_minor) = self.assemble_post(ctx, scope, &req, payer).await?;

        // 4. Post via the invariant engine with the in-txn event sidecar. The
        //    engine's Fresh claim on (tenant, MANUAL_ADJUSTMENT, adjustment_id) is the
        //    idempotency gate; the sidecar publishes the event in the post txn (fresh
        //    post only — a replay returns before the sidecar).
        let sidecar: Arc<dyn PostSidecar> = Arc::new(ManualAdjustmentPostSidecar {
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
            event_template: ManualAdjustmentPosted {
                tenant_id: req.tenant_id,
                adjustment_id: req.adjustment_id.clone(),
                // Placeholder — the sidecar substitutes the posted entry id.
                entry_id: Uuid::nil(),
                action: req.action.as_str().to_owned(),
                reason_code: req.reason_code.clone(),
                actor_ref: req.preparer_actor_id.to_string(),
                amount_minor,
                currency: req.currency.clone(),
            },
        });

        self.posting
            .post(ctx, scope, entry, lines, Some(sidecar))
            .await
    }

    /// Resolve each leg's chart `account_id` + currency scale and assemble the engine
    /// [`NewEntry`] + [`NewLine`] vector, returning the gross adjustment amount
    /// (`Σ DR`, == `Σ CR`) alongside. Per-stream classes resolve on their stream; the
    /// parking / clearing classes resolve stream-less (the chart keys them so). The
    /// header is `source_doc_type = ManualAdjustment` + `source_business_id =
    /// adjustment_id` (the engine's idempotency key).
    async fn assemble_post(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &ManualAdjustmentRequest,
        payer: Uuid,
    ) -> Result<(NewEntry, Vec<NewLine>, i64), DomainError> {
        let chart = load_chart(&self.reference, scope, req.tenant_id).await?;
        let scale = self
            .resolver
            .resolve(scope, req.tenant_id, &req.currency)
            .await
            .map_err(|e| DomainError::Internal(format!("currency scale resolve: {e}")))?;

        let eff_date = to_naive_date(OffsetDateTime::now_utc());
        let period_id = format!("{:04}{:02}", eff_date.year(), eff_date.month());

        let mut lines: Vec<NewLine> = Vec::with_capacity(req.legs.len());
        let mut dr: i128 = 0;
        for leg in &req.legs {
            let account_id = chart
                .resolve(
                    leg.account_class,
                    &req.currency,
                    leg.revenue_stream.as_deref(),
                )
                .ok_or_else(|| {
                    DomainError::AccountClosed(format!(
                        "no provisioned account for class {} / stream {:?} / currency {}",
                        leg.account_class.as_str(),
                        leg.revenue_stream,
                        req.currency
                    ))
                })?;
            if leg.side == Side::Debit {
                dr += i128::from(leg.amount_minor);
            }
            lines.push(Self::mk_line(req, leg, account_id, scale, payer));
        }
        // govern already balanced the legs in i128 and rejected an out-of-i64 set, so
        // the DR total fits i64; guard the cast defensively rather than truncate.
        let amount_minor = i64::try_from(dr).map_err(|_| {
            DomainError::Internal(format!(
                "manual adjustment gross amount {dr} overflows i64 (govern should have rejected)"
            ))
        })?;

        let entry = NewEntry {
            entry_id: Uuid::now_v7(),
            tenant_id: req.tenant_id,
            // v1: one legal entity per tenant — derived server-side.
            legal_entity_id: req.tenant_id,
            period_id,
            entry_currency: req.currency.clone(),
            source_doc_type: SourceDocType::ManualAdjustment,
            // The engine's (tenant, MANUAL_ADJUSTMENT, adjustment_id) idempotency key.
            source_business_id: req.adjustment_id.clone(),
            reverses_entry_id: None,
            reverses_period_id: None,
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: eff_date,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: ctx.subject_id(),
            correlation_id: Uuid::now_v7(),
            rounding_evidence: serde_json::Value::Null,
            // Slice 5: same-currency in v1 (no FX lock on this path).
            rate_snapshot_ref: None,
        };
        Ok((entry, lines, amount_minor))
    }

    /// Map one [`ManualLeg`] + its resolved chart account/scale to the engine
    /// [`NewLine`]. Every leg carries `payer_tenant_id` (the resolved payer — the
    /// request's `payer_tenant_id`, or the tenant itself for a payer-less internal
    /// move) so an `AR` / `UNALLOCATED` grain attributes correctly. The MVP governed
    /// actions move no tax, so the tax dims are `None` and there is no invoice / SKU /
    /// PO linkage.
    fn mk_line(
        req: &ManualAdjustmentRequest,
        leg: &ManualLeg,
        account_id: Uuid,
        scale: u8,
        payer: Uuid,
    ) -> NewLine {
        NewLine {
            line_id: Uuid::now_v7(),
            payer_tenant_id: payer,
            seller_tenant_id: Some(req.tenant_id),
            resource_tenant_id: None,
            account_id,
            account_class: leg.account_class,
            gl_code: None,
            side: leg.side,
            amount_minor: leg.amount_minor,
            currency: req.currency.clone(),
            currency_scale: scale,
            invoice_id: None,
            due_date: None,
            revenue_stream: leg.revenue_stream.clone(),
            mapping_status: MappingStatus::Resolved,
            functional_amount_minor: None,
            functional_currency: None,
            // The MVP governed actions (rounding / suspense clean-up) move no tax, so
            // a manual-adjustment leg carries no tax dimensions.
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

    /// Capture + page an attempted (disguised bad-debt) write-off (design §4.6 / §6
    /// A4) — out-of-band, on the REJECTED post (no books effect occurred):
    ///
    /// - **page** the `AttemptedWriteOff` alarm (`Critical`) so Revenue Assurance /
    ///   Finance Ops is notified of the deliberate-misuse attempt. Fire-and-forget on
    ///   the publisher's own committed connection (mirrors
    ///   [`CreditNoteHandler::emit_split_blocked_alarm`](super::credit_note_service)).
    /// - **capture** the actor in a `SecuredAuditSink` record on a SEPARATE committed
    ///   transaction (the reject has no post txn to ride — mirrors the standalone
    ///   `db.transaction` shape in
    ///   [`allocate`](crate::infra::payment::allocate) with the
    ///   `DbError::Sea(DbErr::Custom(...))` error encoding). The `before_after` payload
    ///   is PII-free (ids + enum codes + amounts only). A capture failure is logged
    ///   and SWALLOWED: it must never mask the original reject (and until Slice 6 the
    ///   no-op sink writes nothing durable anyway).
    async fn capture_and_page_write_off(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        req: &ManualAdjustmentRequest,
        detail: &str,
    ) {
        // (page) The Critical write-off alarm — fire-and-forget on its own committed
        // connection (the post was rejected with NO books effect, so there is no post
        // txn to ride).
        self.publisher
            .emit_invariant_alarm(
                ctx,
                LedgerInvariantAlarm {
                    category: AlarmCategory::AttemptedWriteOff,
                    severity: AlarmSeverity::Critical,
                    tenant_id: req.tenant_id,
                    scope: format!(
                        "tenant:{}/flow:MANUAL_ADJUSTMENT/business:{}",
                        req.tenant_id, req.adjustment_id
                    ),
                    code: CODE_MANUAL_ADJUSTMENT_NOT_ALLOWED.to_owned(),
                    detail: detail.to_owned(), // internal diagnostic — no PII
                    affected: vec![],
                },
            )
            .await;

        // (capture) The secured-audit record on a SEPARATE committed transaction. The
        // before_after is PII-free: ids + the action / reason codes + the per-leg
        // (class, side, amount) — no names / free text.
        let before_after = serde_json::json!({
            "attempted": "MANUAL_ADJUSTMENT_WRITE_OFF",
            "adjustment_id": req.adjustment_id,
            "action": req.action.as_str(),
            "reason_code": req.reason_code,
            "legs": req
                .legs
                .iter()
                .map(|l| serde_json::json!({
                    "account_class": l.account_class.as_str(),
                    "side": l.side.as_str(),
                    "amount_minor": l.amount_minor,
                }))
                .collect::<Vec<_>>(),
        });
        let preparer_actor_str = req.preparer_actor_id.to_string();

        // Own everything the `'static` move closure needs (the closure error type is
        // fixed to DbError, so a sink error is encoded as DbErr::Custom and surfaced
        // after the transaction — mirrors infra/payment/allocate.rs).
        let scope_owned = scope.clone();
        let audit = Arc::clone(&self.audit);
        let tenant = req.tenant_id;
        let result = self
            .db
            .transaction(move |txn| {
                Box::pin(async move {
                    audit
                        .append(
                            txn,
                            &scope_owned,
                            tenant,
                            AuditEventType::ManualAdjustment,
                            Some(preparer_actor_str.as_str()),
                            Some(CODE_MANUAL_ADJUSTMENT_NOT_ALLOWED),
                            &before_after,
                            None,
                            None,
                        )
                        .await
                        .map_err(|e| DbError::Sea(DbErr::Custom(e.to_string())))?;
                    Ok::<(), DbError>(())
                })
            })
            .await;
        if let Err(e) = result {
            // Best-effort: the capture must NOT mask the original reject (and the
            // no-op sink writes nothing durable until Slice 6). Log + swallow.
            tracing::error!(
                tenant_id = %req.tenant_id,
                adjustment_id = %req.adjustment_id,
                error = %e,
                "bss-ledger: attempted-write-off secured-audit capture failed (swallowed; the \
                 reject is unaffected)"
            );
        }
    }
}

/// The in-transaction [`PostSidecar`] for a governed manual adjustment: runs AFTER
/// balance projection and BEFORE the dedup finalize (fresh-claim path only — a
/// replay returns before the sidecar), so the published event commits atomically
/// with the journal entry or rolls back with it (design §4.6). Unlike the
/// credit-note sidecar it does NO schedule / headroom / record write (a manual
/// adjustment touches none) — it ONLY publishes
/// `billing.ledger.manual_adjustment.posted` into the post txn (the transactional
/// outbox). Mirrors [`RefundPostSidecar`](super::refund_service::RefundPostSidecar)'s
/// publish-only tail.
pub struct ManualAdjustmentPostSidecar {
    /// The event publisher: `billing.ledger.manual_adjustment.posted` is published IN
    /// this post txn (the transactional outbox) so it commits atomically with the
    /// entry, or a publish failure rolls the post back.
    publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (cloned by the handler).
    ctx: SecurityContext,
    /// The event payload assembled by the handler (tenant / `adjustment_id` / action /
    /// `reason_code` / `actor_ref` / `amount_minor` / currency). Its `entry_id` is a nil
    /// placeholder — [`Self::run`] substitutes the posted entry id from
    /// [`PostedFacts`].
    event_template: ManualAdjustmentPosted,
}

#[async_trait::async_trait]
impl PostSidecar for ManualAdjustmentPostSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        _scope: &AccessScope,
        posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // Publish `billing.ledger.manual_adjustment.posted` into the SAME post txn
        // (transactional outbox): the event row commits atomically with the entry, or
        // a publish failure rolls the whole post back. Never on replay (a replay
        // returns before the sidecar). Ids + enum codes + amount only (no PII). The
        // posted entry id is stamped here (the template carried a nil placeholder).
        self.publisher
            .publish_manual_adjustment_posted(
                &self.ctx,
                txn,
                ManualAdjustmentPosted {
                    entry_id: posted.entry_id,
                    ..self.event_template.clone()
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish manual_adjustment.posted: {e}")))?;
        Ok(())
    }
}
