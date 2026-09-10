//! [`ScheduleBuilderSidecar`] — the in-transaction [`PostSidecar`] that
//! materializes ASC 606 recognition schedules in the SAME serializable
//! transaction as the invoice post's `CR CONTRACT_LIABILITY` credit (design
//! §4.2 / Group C2/C3). Mirrors the payment
//! [sidecars](crate::infra::payment::sidecar): its writes commit atomically with
//! the journal entry or roll back with it (a derivation that produced a schedule
//! whose insert fails — or a duplicate-build collision — rolls the whole post
//! back, so a deferred Contract-liability balance never exists without its
//! schedule, and a schedule never exists without the balance).
//!
//! For each [`BuiltSchedule`] the pure derivation produced (one per deferred
//! item-stream), [`run`](ScheduleBuilderSidecar::run):
//!
//! 1. **Claims `SCHEDULE_BUILD` idempotency** keyed
//!    `business_id = source_invoice_id:source_invoice_item_ref:revenue_stream`.
//!    `SCHEDULE_BUILD` posts NO journal entry of its own (the invoice post is the
//!    entry); the claim is purely the at-most-once build guard. On a **replay**
//!    (the key is already present — a duplicate build of the same invoice/item/
//!    stream) it **skips** the schedule (the ACTIVE schedule already exists; the
//!    partial UNIQUE is the storage backstop) and does NOT mint a second
//!    `schedule_id`. The claim is never `finalize`d (there is no result entry to
//!    stamp); it stays `CLAIMED` as a permanent build marker.
//! 2. On a **fresh claim**, mints a fresh `schedule_id` (`UUIDv7` string), projects
//!    the [`BuiltSchedule`] into the [`NewSchedule`] + [`NewSegment`] insert
//!    shapes (supplying the posting-context identity it holds — `tenant_id`,
//!    `payer_tenant_id`, `source_invoice_id`, and the schedule's
//!    `source_invoice_item_ref`), and inserts both via [`RecognitionRepo`].
//!
//! A deferred item MUST carry an `invoice_item_ref` (`source_invoice_item_ref`
//! is `NOT NULL` and must resolve to the Contract-liability line this very post
//! created, §4.7) — the orchestrator blocks a deferred item that lacks one
//! BEFORE the post, so every [`PlannedScheduleMaterialization`] here already
//! carries a non-empty ref.

use std::collections::HashMap;
use std::sync::Arc;

use bss_ledger_sdk::SourceDocType;
use toolkit_db::secure::{AccessScope, DbTx};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::RepoError;
use crate::domain::recognition::builder::BuiltSchedule;
use crate::infra::events::payloads::{LedgerRevenueRecognitionReversed, LedgerRevenueRecognized};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::posting::idempotency::{ClaimOutcome, IdempotencyGate};
use crate::infra::posting::service::{PostSidecar, PostedFacts};
use crate::infra::storage::repo::RecognitionRepo;
use crate::infra::storage::repo::recognition_repo::{NewSchedule, NewSegment};
use time::OffsetDateTime;

/// One schedule to materialize: the pure [`BuiltSchedule`] plan plus the
/// `source_invoice_item_ref` it draws down (the Contract-liability line this post
/// created, §4.7). The orchestrator pairs each derived schedule with its item's
/// ref (asserted non-empty before the post) and the sidecar projects the pair
/// into the storage rows.
#[derive(Clone, Debug)]
pub struct PlannedScheduleMaterialization {
    /// The derived schedule plan (deferred amount + segments + stamped refs).
    pub schedule: BuiltSchedule,
    /// The deferred item's `invoice_item_ref` — the `recognition_schedule`
    /// `source_invoice_item_ref` (NOT NULL); non-empty by orchestrator invariant.
    pub source_invoice_item_ref: String,
}

/// In-transaction sidecar that materializes the derived recognition schedules.
/// Holds the plans + the posting-context identity common to all of them (the
/// per-schedule identity lives on each [`PlannedScheduleMaterialization`]).
pub struct ScheduleBuilderSidecar {
    /// The seller tenant whose ledger this posts into (`= entry.tenant_id`).
    pub tenant_id: Uuid,
    /// The tenant that pays the invoice (the schedule's `payer_tenant_id`).
    pub payer_tenant_id: Uuid,
    /// The external invoice id (the schedule's `source_invoice_id` + the first
    /// segment of the `SCHEDULE_BUILD` dedup business id).
    pub source_invoice_id: String,
    /// The schedules to materialize (one per deferred item-stream).
    pub schedules: Vec<PlannedScheduleMaterialization>,
    /// The at-most-once build gate (claims `SCHEDULE_BUILD`).
    pub idempotency: IdempotencyGate,
    /// Discriminates a later EXTEND build (a debit note adding deferred to a live
    /// schedule) from the FIRST build (invoice-post): `None` for the invoice-post
    /// (mints the schedule), `Some(note_id)` for a debit note — so its
    /// `SCHEDULE_BUILD` claim does not collide with (and replay → skip) the base
    /// build, and it EXTENDS the live schedule instead of minting a second one the
    /// partial UNIQUE would reject.
    pub build_discriminator: Option<String>,
}

impl ScheduleBuilderSidecar {
    /// The `idempotency_dedup` business id for one schedule build:
    /// `source_invoice_id:source_invoice_item_ref:revenue_stream` (design §3.2),
    /// suffixed with the `build_discriminator` (a debit note's id) when set so an
    /// EXTEND build does not collide with (and replay → skip) the base build. One
    /// schedule per stream, so the stream tail keeps a multi-stream invoice's
    /// builds distinct.
    fn build_business_id(&self, item_ref: &str, revenue_stream: &str) -> String {
        match &self.build_discriminator {
            Some(d) => format!("{}:{item_ref}:{revenue_stream}:{d}", self.source_invoice_id),
            None => format!("{}:{item_ref}:{revenue_stream}", self.source_invoice_id),
        }
    }

    /// Project one [`BuiltSchedule`] + its `source_invoice_item_ref` into the
    /// repo insert shapes, minting the supplied `schedule_id`. Pure (no I/O); the
    /// caller runs the inserts.
    fn project(
        &self,
        schedule_id: &str,
        plan: &PlannedScheduleMaterialization,
    ) -> (NewSchedule, Vec<NewSegment>) {
        let s = &plan.schedule;
        let new_schedule = NewSchedule {
            tenant_id: self.tenant_id,
            schedule_id: schedule_id.to_owned(),
            payer_tenant_id: self.payer_tenant_id,
            source_invoice_id: self.source_invoice_id.clone(),
            source_invoice_item_ref: plan.source_invoice_item_ref.clone(),
            po_allocation_group: s.po_allocation_group.clone(),
            subscription_ref: s.subscription_ref.clone(),
            revenue_stream: s.revenue_stream.clone(),
            currency: s.currency.clone(),
            total_deferred_minor: s.deferred_minor,
            policy_ref: s.policy_ref.clone(),
            ssp_snapshot_ref: s.ssp_snapshot_ref.clone(),
            vc_estimate_ref: s.vc_estimate_ref.clone(),
            vc_method_ref: s.vc_method_ref.clone(),
        };
        let segments: Vec<NewSegment> = s
            .segments
            .iter()
            .map(|seg| NewSegment {
                tenant_id: self.tenant_id,
                schedule_id: schedule_id.to_owned(),
                segment_no: seg.segment_no,
                period_id: seg.period_id.clone(),
                amount_minor: seg.amount_minor,
            })
            .collect();
        (new_schedule, segments)
    }

    /// EXTEND a live ACTIVE schedule with a later note's deferred part: add to its
    /// `total_deferred_minor` and MERGE the note's segments — fold the amount into
    /// an existing PENDING period, else append a fresh segment (continuing
    /// `segment_no` past the current max). One ACTIVE schedule per key is preserved
    /// (the partial UNIQUE), so the credit-note splitter + the recognition runner
    /// see ONE aggregate releasable balance, not a skipped second schedule.
    /// Extending a period already released / parked (non-`PENDING`) is rejected by
    /// `add_pending_segment_amount` (rolls the post back) — a debit note normally
    /// lands before the base schedule's first release.
    async fn extend(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        schedule_id: &str,
        plan: &PlannedScheduleMaterialization,
    ) -> Result<(), DomainError> {
        let s = &plan.schedule;
        RecognitionRepo::increase_total_deferred(
            txn,
            scope,
            self.tenant_id,
            schedule_id,
            s.deferred_minor,
        )
        .await
        .map_err(|e| DomainError::Internal(format!("extend total_deferred: {e}")))?;

        let existing =
            RecognitionRepo::list_segments_in_txn(txn, scope, self.tenant_id, schedule_id)
                .await
                .map_err(|e| DomainError::Internal(format!("list segments for extend: {e}")))?;
        let by_period: HashMap<&str, i32> = existing
            .iter()
            .map(|r| (r.period_id.as_str(), r.segment_no))
            .collect();
        let mut next_no = existing.iter().map(|r| r.segment_no).max().unwrap_or(0) + 1;

        for seg in &s.segments {
            if let Some(&segment_no) = by_period.get(seg.period_id.as_str()) {
                RecognitionRepo::add_pending_segment_amount(
                    txn,
                    scope,
                    self.tenant_id,
                    schedule_id,
                    segment_no,
                    seg.amount_minor,
                )
                .await
                .map_err(|e| DomainError::Internal(format!("extend segment: {e}")))?;
            } else {
                let appended = vec![NewSegment {
                    tenant_id: self.tenant_id,
                    schedule_id: schedule_id.to_owned(),
                    segment_no: next_no,
                    period_id: seg.period_id.clone(),
                    amount_minor: seg.amount_minor,
                }];
                RecognitionRepo::insert_segments(txn, scope, &appended)
                    .await
                    .map_err(|e| DomainError::Internal(format!("append extend segment: {e}")))?;
                next_no += 1;
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PostSidecar for ScheduleBuilderSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        _posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        let flow = SourceDocType::ScheduleBuild.as_str();
        for plan in &self.schedules {
            let business_id = self
                .build_business_id(&plan.source_invoice_item_ref, &plan.schedule.revenue_stream);

            // Claim the SCHEDULE_BUILD key. There is no journal entry of its own
            // for this flow, so the payload hash is over the build business id
            // (stable across retries). A `Replay` means the schedule was already
            // built (a duplicate build) — skip; the ACTIVE schedule exists.
            let payload_hash = IdempotencyGate::content_hash(&business_id);
            match self
                .idempotency
                .claim(txn, self.tenant_id, flow, &business_id, &payload_hash)
                .await
                .map_err(|e| {
                    DomainError::Internal(format!("schedule-build idempotency claim: {e}"))
                })? {
                ClaimOutcome::Replay(_) => continue,
                ClaimOutcome::Claimed => {}
            }

            // Fresh claim: EXTEND the live schedule if one exists for this key (a
            // later deferring note — a debit note — adds its deferred part to it;
            // one ACTIVE schedule per key, the partial UNIQUE), else mint the FIRST
            // schedule (the invoice-post). A failure rolls the whole post back.
            if let Some(existing) = RecognitionRepo::read_active_schedule_in_txn(
                txn,
                scope,
                self.tenant_id,
                &self.source_invoice_id,
                &plan.source_invoice_item_ref,
                &plan.schedule.revenue_stream,
            )
            .await
            .map_err(|e| DomainError::Internal(format!("read active schedule: {e}")))?
            {
                self.extend(txn, scope, &existing.schedule_id, plan).await?;
            } else {
                let schedule_id = Uuid::now_v7().to_string();
                let (new_schedule, segments) = self.project(&schedule_id, plan);
                RecognitionRepo::insert_schedule(txn, scope, &new_schedule)
                    .await
                    .map_err(|e| {
                        DomainError::Internal(format!("insert recognition_schedule: {e}"))
                    })?;
                RecognitionRepo::insert_segments(txn, scope, &segments)
                    .await
                    .map_err(|e| {
                        DomainError::Internal(format!("insert recognition_segment: {e}"))
                    })?;
            }
        }
        Ok(())
    }
}

/// In-transaction [`PostSidecar`] for one released recognition segment (design
/// §4.3, Group D2). Threaded by the [`RecognitionRunner`](super::runner) into the
/// `DR CONTRACT_LIABILITY / CR REVENUE` post so the journal entry, the
/// `recognized_minor += amount` counter bump, and the segment `→ DONE` stamp
/// commit atomically in the SAME serializable transaction (or roll back
/// together) — the post engine runs this AFTER balance projection and BEFORE the
/// dedup finalize, on the fresh-claim path only (a `RECOGNITION` replay returns
/// before the sidecar, so a re-credit is structurally impossible).
///
/// **Lock order (design §2 / §4.3).** The post's projection already locked the
/// `CONTRACT_LIABILITY` + `REVENUE` `account_balance` rows (rank 0). This sidecar
/// then takes the recognition rows in the global rank order: **`recognition_schedule`
/// (the `recognized_minor` delta, rank 6) BEFORE `recognition_segment` (the
/// `DONE` stamp, rank 7)** — acquire schedule before segment, one consistent
/// order across all recognition posts, so concurrent runs serialize and never
/// deadlock.
///
/// **Over-recognition guard.** `add_recognized`'s per-schedule
/// `recognized_minor <= total_deferred_minor` cap CHECK is the authoritative,
/// in-txn, lock-ordered guard; a breach surfaces from the repo as
/// [`RepoError::MoneyOutCapExceeded`], which this sidecar refines to
/// [`DomainError::OverRecognition`] (the `OVER_RECOGNITION` 409). The post engine
/// encodes that as a non-retryable business rejection and rolls the whole release
/// back — the counter is never advanced past the deferred total.
pub struct RecognitionStampSidecar {
    /// The seller tenant whose ledger this releases into (`= entry.tenant_id`).
    pub tenant_id: Uuid,
    /// The owning schedule's id (the `recognized_minor` counter grain + the first
    /// segment of the `RECOGNITION` dedup business id).
    pub schedule_id: String,
    /// The released segment's number (immutable, 1:1 with `period_id`).
    pub segment_no: i32,
    /// The accounting period the recognized revenue lands in (`YYYYMM`) — the
    /// release entry's period (the segment's own, or the current-open period on
    /// an E-2 missed-close reassignment). Carried only for the
    /// `billing.ledger.revenue.recognized` event payload.
    pub period_id: String,
    /// The segment's amount released this post (`= the entry's DR/CR amount`),
    /// added to `recognized_minor` under the cap CHECK.
    pub amount_minor: i64,
    /// The revenue stream both legs draw (per-stream disaggregation). Carried for
    /// the recognized-event payload.
    pub revenue_stream: String,
    /// ISO-4217 currency of the release entry. Carried for the recognized-event
    /// payload.
    pub currency: String,
    /// The run that released this segment (stamped on the segment row for audit
    /// linkage).
    pub run_id: Uuid,
    /// The event publisher: `billing.ledger.revenue.recognized` is published IN
    /// this post txn (the transactional outbox) so it commits atomically with the
    /// release entry + the counter bump + the segment `DONE` stamp, or rolls back
    /// with them. Mirrors the payment
    /// [sidecars](crate::infra::payment::sidecar).
    pub publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (the same `ctx` the
    /// engine threads through; cloned by the runner into the sidecar).
    pub ctx: SecurityContext,
}

#[async_trait::async_trait]
impl PostSidecar for RecognitionStampSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        _posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // 1. Schedule first (rank 6): bump `recognized_minor` by the released
        //    amount. The per-schedule cap CHECK is the SERIALIZABLE backstop — an
        //    over-release surfaces as `MoneyOutCapExceeded`, refined to
        //    `OverRecognition` (409). A replay returned before the sidecar, so this
        //    is reached only on the first release of `(schedule, segment)`.
        RecognitionRepo::add_recognized(
            txn,
            scope,
            self.tenant_id,
            &self.schedule_id,
            self.amount_minor,
        )
        .await
        .map_err(map_recognition_repo_err)?;

        // 2. Segment next (rank 7): flip PENDING/QUEUED → DONE, stamping
        //    `recognized_at` (the infra wall clock — `OffsetDateTime::now_utc()` is allowed in
        //    infra, mirroring the payment sidecars' `allocated_at_utc`) + `run_id`.
        //    The status filter refuses an already-DONE row, so a stray re-stamp on
        //    the fresh-claim path is an invariant breach that rolls the post back
        //    (`RepoError::Db` → `Internal`) rather than silently double-crediting.
        RecognitionRepo::stamp_segment_done(
            txn,
            scope,
            self.tenant_id,
            &self.schedule_id,
            self.segment_no,
            self.run_id,
            OffsetDateTime::now_utc(),
        )
        .await
        .map_err(map_recognition_repo_err)?;

        // 3. Terminal completion (design §4.6): if THIS release drained the
        //    schedule (`recognized_minor == total_deferred_minor` after the bump
        //    above, all segments DONE), flip it `ACTIVE → COMPLETED` in the SAME
        //    txn — freeing the partial one-live UNIQUE slot, dropping it from the
        //    runner's ACTIVE-only feed + the `schedule_active_total` gauge. The
        //    filter is column-to-column equality, so this is a no-op on every
        //    non-final release and on a replay (idempotent); it never bumps
        //    `version` (COMPLETED is the same schedule reaching terminal, not a
        //    new lineage). RELEASE path only — the reversal sidecar must NOT
        //    complete (a reversal un-drains the schedule).
        RecognitionRepo::complete_schedule_if_drained(
            txn,
            scope,
            self.tenant_id,
            &self.schedule_id,
        )
        .await
        .map_err(map_recognition_repo_err)?;

        // 4. Publish `billing.ledger.revenue.recognized` into the SAME post txn
        //    (transactional outbox): the event row commits atomically with the
        //    release entry + the counter bump + the segment `DONE` stamp, or a
        //    publish failure rolls the whole release back. Ids + amount + stream +
        //    period only (no PII). Reached only on the fresh-claim path (a replay
        //    returns before the sidecar), so the event fires once per release.
        self.publisher
            .publish_revenue_recognized(
                &self.ctx,
                txn,
                LedgerRevenueRecognized {
                    tenant_id: self.tenant_id,
                    schedule_id: self.schedule_id.clone(),
                    segment_no: self.segment_no,
                    period_id: self.period_id.clone(),
                    amount_minor: self.amount_minor,
                    revenue_stream: self.revenue_stream.clone(),
                    currency: self.currency.clone(),
                },
            )
            .await
            .map_err(|e| DomainError::Internal(format!("publish revenue_recognized: {e}")))?;

        Ok(())
    }
}

/// Map a recognition-counter [`RepoError`] into the sidecar's [`DomainError`]:
/// the per-schedule `recognized_minor <= total_deferred_minor` cap CHECK
/// violation (`add_recognized`) becomes [`DomainError::OverRecognition`] (the
/// `OVER_RECOGNITION` 409 — design §4.3 / §5); every other repo failure (incl.
/// the `stamp_segment_done` `rows_affected == 0` invariant breach) is an
/// infrastructure fault whose diagnostic stays server-side and rolls the post
/// back. Mirrors the payment sidecars' `map_*_repo_err` shape.
fn map_recognition_repo_err(e: RepoError) -> DomainError {
    match e {
        RepoError::MoneyOutCapExceeded(m) => DomainError::OverRecognition(m),
        other => DomainError::Internal(format!("recognition stamp sidecar: {other}")),
    }
}

/// In-transaction [`PostSidecar`] for one recognition **reversal / clawback**
/// (design §4.3, Group F1). Threaded by the [`RecognitionRunner`](super::runner)
/// into the compensating `DR REVENUE / CR CONTRACT_LIABILITY` post so the
/// reversing journal entry and the `recognized_minor -= amount` counter
/// **decrement** commit in the SAME serializable transaction (or roll back
/// together). The reversal is the mirror of [`RecognitionStampSidecar`]: it
/// posts the opposite legs and applies a NEGATIVE delta to `recognized_minor`.
///
/// **The reversed segment stays `DONE` (design §4.3).** A reversal compensates a
/// release that genuinely happened; the segment's release is a historical fact,
/// so its `recognition_segment` row is left untouched (`status = DONE`,
/// `recognized_at`/`run_id` preserved). Re-recognizing the period needs a NEW
/// schedule version (a fresh `schedule_id`, Phase 3) — never a re-flip of this
/// segment back to `PENDING`. This sidecar therefore writes ONLY the counter
/// decrement; it does not touch the segment row.
///
/// **Lock order (design §2 / §4.3).** Same as the release: the post's projection
/// already locked the `REVENUE` + `CONTRACT_LIABILITY` `account_balance` rows
/// (rank 0); this sidecar then takes only the `recognition_schedule` row (the
/// `recognized_minor` delta, rank 6). It never touches `recognition_segment`
/// (rank 7), so it acquires a strict prefix of the release's lock set — no new
/// ordering edge, no deadlock.
///
/// **Underflow guard.** `add_recognized` with a negative delta is guarded by the
/// per-schedule `recognized_minor >= 0` cap CHECK
/// (`chk_ledger_recognition_schedule_recognized_nonneg`): a reversal larger than
/// the cumulative recognized would drive the counter below zero and is rejected,
/// surfacing from the repo as [`RepoError::MoneyOutCapExceeded`] (both schedule
/// CHECKs share the `chk_ledger_recognition_schedule_` prefix the repo's
/// violation classifier keys on). This sidecar refines that to
/// [`DomainError::OverRecognition`] with a reversal-specific detail — a reversal
/// can never un-recognize more than was recognized.
pub struct RecognitionReversalSidecar {
    /// The seller tenant whose ledger this reverses within (`= entry.tenant_id`).
    pub tenant_id: Uuid,
    /// The owning schedule's id (the `recognized_minor` counter grain + the first
    /// segment of the `RECOGNITION` reversal dedup business id).
    pub schedule_id: String,
    /// The reversed segment's number (the segment stays `DONE`). Carried for the
    /// `billing.ledger.revenue.recognition_reversed` event payload.
    pub segment_no: i32,
    /// The accounting period the reversal lands in (`YYYYMM`). Carried for the
    /// reversed-event payload.
    pub period_id: String,
    /// The segment amount being reversed (`= the entry's DR/CR amount`),
    /// SUBTRACTED from `recognized_minor` under the non-negative cap CHECK.
    pub amount_minor: i64,
    /// The revenue stream both legs draw. Carried for the reversed-event payload.
    pub revenue_stream: String,
    /// ISO-4217 currency of the reversal entry. Carried for the reversed-event
    /// payload.
    pub currency: String,
    /// The event publisher: `billing.ledger.revenue.recognition_reversed` is
    /// published IN this post txn (the transactional outbox) so it commits
    /// atomically with the reversing entry + the counter decrement, or rolls back
    /// with them. Mirrors [`RecognitionStampSidecar`].
    pub publisher: Arc<LedgerEventPublisher>,
    /// The security context for the in-txn outbox publish (the same `ctx` the
    /// engine threads through; cloned by the runner into the sidecar).
    pub ctx: SecurityContext,
}

#[async_trait::async_trait]
impl PostSidecar for RecognitionReversalSidecar {
    async fn run(
        &self,
        txn: &DbTx<'_>,
        scope: &AccessScope,
        _posted: &PostedFacts,
    ) -> Result<(), DomainError> {
        // Schedule only (rank 6): DECREMENT `recognized_minor` by the reversed
        // amount (a negative delta). The per-schedule `recognized_minor >= 0`
        // CHECK is the SERIALIZABLE backstop — an over-reversal surfaces as
        // `MoneyOutCapExceeded`, refined to `OverRecognition` (the reversal cannot
        // un-recognize more than was recognized). The segment row is NOT touched:
        // the reversed segment stays `DONE` (design §4.3). A `RECOGNITION` reversal
        // replay returned before the sidecar, so this runs once per
        // `(schedule, segment, reversal)`.
        RecognitionRepo::add_recognized(
            txn,
            scope,
            self.tenant_id,
            &self.schedule_id,
            -self.amount_minor,
        )
        .await
        .map_err(map_recognition_repo_err)?;

        // Publish `billing.ledger.revenue.recognition_reversed` into the SAME post
        // txn (transactional outbox): the event row commits atomically with the
        // reversing entry + the counter decrement, or a publish failure rolls the
        // whole reversal back. Ids + amount + stream + period only (no PII). A
        // reversal replay returns before the sidecar, so the event fires once per
        // `(schedule, segment, reversal)`.
        self.publisher
            .publish_revenue_recognition_reversed(
                &self.ctx,
                txn,
                LedgerRevenueRecognitionReversed {
                    tenant_id: self.tenant_id,
                    schedule_id: self.schedule_id.clone(),
                    segment_no: self.segment_no,
                    period_id: self.period_id.clone(),
                    // Signed delta to cumulative recognized revenue: NEGATIVE on a
                    // reversal, mirroring the counter decrement
                    // above. A consumer nets `recognized` against `recognition_reversed`
                    // by summing `amount_minor` across both, without special-casing
                    // the event type-id.
                    amount_minor: -self.amount_minor,
                    revenue_stream: self.revenue_stream.clone(),
                    currency: self.currency.clone(),
                },
            )
            .await
            .map_err(|e| {
                DomainError::Internal(format!("publish revenue_recognition_reversed: {e}"))
            })?;
        Ok(())
    }
}
