//! `LedgerApprovalExecutor` — the concrete [`ApprovalExecutor`] that replays an
//! approved [`ApprovalIntent`] against the real mutation surfaces (VHP-1852,
//! Group E). Reverse goes through the posting engine (read-back the original →
//! `build_reversal` → `post_reversal`); credit-grant and chargeback-loss go
//! through the `LedgerClientV1` surfaces (which re-apply their own PEP gate +
//! idempotency). Replay is idempotent: a re-approve short-circuits on the
//! foundation idempotency key, so execute-then-mark is safe.

use std::sync::Arc;

use bss_ledger_sdk::{
    ChangeRecognitionSchedule, ChangeSegment, CreditApplication, CreditGrant, LedgerClientV1,
    RecordDisputePhase,
};
use toolkit_db::secure::AccessScope;
use toolkit_security::SecurityContext;

use crate::domain::adjustment::manual::ManualAdjustmentRequest;
use crate::domain::adjustment::refund::RefundRequest;
use crate::domain::approval::intent::{ApprovalIntent, BackdatedPost};
use crate::domain::error::DomainError;
use crate::domain::instant::to_naive_date;
use crate::domain::invoice::builder::PostedInvoice;
use crate::domain::invoice::reversal::build_reversal;
use crate::domain::payment::chargeback::DisputePhase;
use crate::infra::adjustment::credit_note_service::CreditNoteHandler;
use crate::infra::adjustment::debit_note_service::DebitNoteHandler;
use crate::infra::adjustment::manual_adjustment_service::ManualAdjustmentHandler;
use crate::infra::adjustment::refund_service::RefundHandler;
use crate::infra::approval::service::ApprovalExecutor;
use crate::infra::invoice_post::InvoicePoster;
use crate::infra::period_close::PeriodCloseService;
use crate::infra::storage::repo::PayerStateRepo;
use time::OffsetDateTime;

/// Default advisory currency scale passed on replayed commands; the ledger
/// resolves the authoritative per-line scale from the provisioned currency config.
const ADVISORY_SCALE: u8 = 2;

/// Dispatches an approved intent to the real mutation surface.
pub struct LedgerApprovalExecutor {
    client: Arc<dyn LedgerClientV1>,
    posting: Arc<dyn InvoicePoster>,
    payer_state: PayerStateRepo,
    /// The refund orchestrator (Group D). A refund is NOT a `LedgerClientV1`
    /// surface (it is a concrete handler, like the notes), so an approved refund
    /// replays straight through it via [`RefundHandler::post_refund_approved`]
    /// (which skips the gate). Kept the un-gated `RefundHandler` (no `approval`
    /// attached) so the replay can never re-gate.
    refund: Arc<RefundHandler>,
    /// The governed manual-adjustment orchestrator (Group 5 / Phase 3). Like
    /// [`Self::refund`], a manual adjustment is NOT a `LedgerClientV1` surface (it is a
    /// concrete handler), so an approved manual adjustment replays straight through it
    /// via [`ManualAdjustmentHandler::post_manual_adjustment_approved`] (which skips
    /// the gate). Kept the un-gated `ManualAdjustmentHandler` (no `approval` attached)
    /// so the replay can never re-gate.
    manual: Arc<ManualAdjustmentHandler>,
    /// The credit-note orchestrator (Group E / Slice 3). Like [`Self::refund`], a
    /// credit note is NOT a `LedgerClientV1` surface (it is a concrete handler), so an
    /// approved credit note replays straight through it via
    /// [`CreditNoteHandler::post_credit_note_approved`], which skips the dual-control
    /// gate via an internal `gate=false` flag REGARDLESS of whether the handler has an
    /// `approval` wired. This is therefore the SAME gated instance the REST surface
    /// uses (a gated handler serves both the inline gated path and this replay) — the
    /// `*_approved` entry never re-gates.
    credit_note: Arc<CreditNoteHandler>,
    /// The debit-note orchestrator (Group E / Slice 3). Like [`Self::credit_note`], a
    /// debit note is NOT a `LedgerClientV1` surface (it is a concrete handler), so an
    /// approved debit note replays straight through it via
    /// [`DebitNoteHandler::post_debit_note_approved`], which skips the dual-control gate
    /// via an internal `gate=false` flag REGARDLESS of whether the handler has an
    /// `approval` wired. This is therefore the SAME gated instance the REST surface uses
    /// — the `*_approved` entry never re-gates.
    debit_note: Arc<DebitNoteHandler>,
    /// The period-close service (Slice 7). An approved `PeriodReopen` replays
    /// through [`PeriodCloseService::reopen`] (CLOSED→REOPENED flip + secured
    /// `period-reopen` audit) — the only reopen path (reopen is always
    /// dual-control, so there is no inline reopen).
    period_close: PeriodCloseService,
}

impl LedgerApprovalExecutor {
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "the executor replays each approved mutation kind through its own handler — one handler per dual-control approval kind"
    )]
    pub fn new(
        client: Arc<dyn LedgerClientV1>,
        posting: Arc<dyn InvoicePoster>,
        payer_state: PayerStateRepo,
        refund: Arc<RefundHandler>,
        manual: Arc<ManualAdjustmentHandler>,
        credit_note: Arc<CreditNoteHandler>,
        debit_note: Arc<DebitNoteHandler>,
        period_close: PeriodCloseService,
    ) -> Self {
        Self {
            client,
            posting,
            payer_state,
            refund,
            manual,
            credit_note,
            debit_note,
            period_close,
        }
    }
}

#[async_trait::async_trait]
impl ApprovalExecutor for LedgerApprovalExecutor {
    async fn execute(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        intent: &ApprovalIntent,
    ) -> Result<(), DomainError> {
        match intent {
            ApprovalIntent::Reverse(i) => {
                let tenant = ctx.subject_tenant_id();
                let original = self
                    .client
                    .get_entry(ctx, tenant, i.entry_id)
                    .await
                    .map_err(|e| DomainError::Internal(format!("get_entry on approve: {e:?}")))?
                    .ok_or_else(|| {
                        DomainError::ApprovalNotActionable(format!(
                            "reversed entry {} no longer exists",
                            i.entry_id
                        ))
                    })?;
                let into_period = i
                    .into_period_id
                    .clone()
                    .unwrap_or_else(|| original.period_id.clone());
                let effective_on = i
                    .effective_at
                    .unwrap_or_else(|| to_naive_date(OffsetDateTime::now_utc()));
                let reversal = build_reversal(
                    &original,
                    into_period,
                    effective_on,
                    ctx.subject_id(),
                    original.correlation_id,
                )
                .map_err(|e| DomainError::Internal(format!("build_reversal on approve: {e:?}")))?;
                // Approved explicit reversal — announce `entry.reversed` (VHP-1837)
                // with the intent's audit reason.
                self.posting
                    .post_reversal(ctx, scope, reversal, Some(i.reason.clone()))
                    .await?;
                Ok(())
            }
            ApprovalIntent::CreditGrant(i) => {
                let event_type = i.credit_grant_event_type.clone().ok_or_else(|| {
                    DomainError::Internal(
                        "credit-grant approval intent missing credit_grant_event_type".to_owned(),
                    )
                })?;
                let cmd = CreditApplication::Grant(CreditGrant {
                    tenant_id: i.tenant_id,
                    payer_tenant_id: i.payer_tenant_id,
                    credit_application_id: i.credit_application_id.clone(),
                    currency: i.currency.clone(),
                    scale: ADVISORY_SCALE,
                    amount_minor: i.amount_minor,
                    credit_grant_event_type: event_type,
                });
                self.client
                    .post_credit_application(ctx, cmd)
                    .await
                    .map_err(|e| {
                        DomainError::Internal(format!("credit grant on approve: {e:?}"))
                    })?;
                Ok(())
            }
            ApprovalIntent::ChargebackLoss(i) => {
                let cmd = RecordDisputePhase {
                    tenant_id: i.tenant_id,
                    payer_tenant_id: i.payer_tenant_id,
                    payment_id: i.payment_id.clone(),
                    dispute_id: i.dispute_id.clone(),
                    invoice_id: i.invoice_id.clone(),
                    cycle: i.cycle,
                    phase: DisputePhase::Lost.as_str().to_owned(),
                    funds_at_open: i.funds_at_open.clone(),
                    disputed_amount_minor: i.disputed_amount_minor,
                    currency: i.currency.clone(),
                    scale: ADVISORY_SCALE,
                    effective_at: None,
                };
                self.client
                    .record_dispute_phase(ctx, cmd)
                    .await
                    .map_err(|e| {
                        DomainError::Internal(format!("chargeback loss on approve: {e:?}"))
                    })?;
                Ok(())
            }
            ApprovalIntent::PayerClosure(i) => {
                self.payer_state
                    .close(
                        scope,
                        i.tenant_id,
                        i.payer_tenant_id,
                        ctx.subject_id(),
                        i.closed_with_open_balance,
                    )
                    .await?;
                Ok(())
            }
            // Slice 7: an approved period-reopen flips CLOSED→OPEN under its own
            // single-active lease + SERIALIZABLE txn and writes the `period-reopen`
            // secured-audit record (idempotent on an already-OPEN period).
            ApprovalIntent::PeriodReopen(i) => {
                self.period_close
                    .reopen(
                        i.tenant_id,
                        i.legal_entity_id,
                        &i.period_id,
                        ctx.subject_id(),
                    )
                    .await?;
                Ok(())
            }
            // Group J: replay a materially-backdated post against its surface. The
            // gate captured the whole external post (the object did not exist at
            // gate time); rebuild the domain primitive and post it idempotently
            // (`payer_open = true`, matching the inline post seam — the foundation
            // account-lifecycle invariant is the authority on a closed payer).
            ApprovalIntent::MaterialBackdating(post) => match post {
                BackdatedPost::Invoice(snap) => {
                    let posted = PostedInvoice::try_from(snap)?;
                    self.posting.post_invoice(ctx, scope, &posted, true).await?;
                    Ok(())
                }
            },
            // Replay the recognition schedule change/cancel through the client
            // (which re-applies its own PEP gate + is idempotent on `change_id`, so
            // a re-approve replays harmlessly — execute-then-mark is safe).
            ApprovalIntent::RecognitionScheduleChange(i) => {
                let cmd = ChangeRecognitionSchedule {
                    tenant_id: i.tenant_id,
                    schedule_id: i.schedule_id.clone(),
                    change_id: i.change_id.clone(),
                    action: i.action.clone(),
                    treatment: i.treatment.clone(),
                    new_segments: i.new_segments.as_ref().map(|segs| {
                        segs.iter()
                            .map(|s| ChangeSegment {
                                period_id: s.period_id.clone(),
                                amount_minor: s.amount_minor,
                            })
                            .collect()
                    }),
                };
                self.client
                    .change_recognition_schedule(ctx, cmd)
                    .await
                    .map_err(|e| {
                        DomainError::Internal(format!(
                            "recognition schedule change on approve: {e:?}"
                        ))
                    })?;
                Ok(())
            }
            // Group D: replay an approved over-threshold refund through the refund
            // orchestrator's APPROVED entry (skips the gate — the threshold was
            // already crossed at gate time). Rebuild the `RefundRequest` from the
            // snapshot and post it idempotently (the engine's
            // `(tenant, REFUND, psp_refund_id:phase)` claim short-circuits a committed
            // post, so a re-approve replays harmlessly — execute-then-mark is safe).
            ApprovalIntent::Refund(i) => {
                let req = RefundRequest::try_from(i)?;
                match self.refund.post_refund_approved(ctx, scope, req).await {
                    Ok(_) => Ok(()),
                    // Z5-2: a dispute opened on the origin payment between gate time
                    // and this replay ⇒ the refund's cash leg was durably HELD on the
                    // `REFUND_DISPUTE_HOLD` queue (it must NOT pay out while the
                    // dispute is sub judice). This is a successful DEFERRAL, not an
                    // executor failure: the approval decision stands (mark APPROVED),
                    // and the dispute-hold drain owns the eventual post (re-driving on
                    // WON / cancelling on LOST — never double-paying). Reverting the
                    // approval to PENDING here would orphan the approver's decision and
                    // race the hold drain. Mirrors how the gate's own deferral signals
                    // are non-failures.
                    Err(DomainError::RefundDisputeHeld(token)) => {
                        tracing::info!(
                            %token,
                            "bss-ledger: approved refund held behind an open dispute on replay — \
                             durably enqueued on REFUND_DISPUTE_HOLD; approval marked APPROVED, the \
                             hold drain owns the post"
                        );
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
            // Group 5 / Phase 3: replay an approved over-threshold governed manual
            // adjustment through the orchestrator's APPROVED entry (skips the gate —
            // the threshold was already crossed at gate time). Rebuild the
            // `ManualAdjustmentRequest` from the snapshot and post it idempotently (the
            // engine's `(tenant, MANUAL_ADJUSTMENT, adjustment_id)` claim short-circuits
            // a committed post, so a re-approve replays harmlessly — execute-then-mark
            // is safe).
            ApprovalIntent::ManualAdjustment(i) => {
                let req = ManualAdjustmentRequest::try_from(i)?;
                self.manual
                    .post_manual_adjustment_approved(ctx, scope, req)
                    .await?;
                Ok(())
            }
            // Group E / Slice 3: replay an approved over-threshold credit note through
            // the orchestrator's APPROVED entry (skips the gate — the threshold was
            // already crossed at gate time). Rebuild the `CreditNoteRequest` from the
            // intent mirror and post it idempotently (a re-approve short-circuits on the
            // foundation idempotency key, so execute-then-mark is safe).
            ApprovalIntent::CreditNote(i) => {
                let req = crate::domain::adjustment::credit_note::CreditNoteRequest::from(i);
                self.credit_note
                    .post_credit_note_approved(ctx, scope, req)
                    .await?;
                Ok(())
            }
            // Group E / Slice 3: replay an approved over-threshold debit note through the
            // orchestrator's APPROVED entry (skips the gate). Rebuild the
            // `DebitNoteRequest` from the intent mirror and post it idempotently.
            ApprovalIntent::DebitNote(i) => {
                let req = crate::domain::adjustment::debit_note::DebitNoteRequest::from(i);
                // payer_open = true: the foundation closed-AR-account invariant is the
                // authority on a since-closed payer (matching the MaterialBackdating seam).
                self.debit_note
                    .post_debit_note_approved(ctx, scope, req, true)
                    .await?;
                Ok(())
            }
            // Group E / Slice 3: replay an approved `refund-with-credit-note` composite
            // through the refund orchestrator's APPROVED entry. Split the intent into its
            // (`RefundRequest`, `CreditNoteRequest`) pair and re-drive the atomic post
            // (the composite uses `apply_in_txn`, which never gates).
            ApprovalIntent::RefundWithCreditNote(i) => {
                let (refund, credit_note) = i.to_requests()?;
                self.refund
                    .post_refund_with_credit_note_approved(ctx, scope, refund, credit_note)
                    .await?;
                Ok(())
            }
        }
    }
}
