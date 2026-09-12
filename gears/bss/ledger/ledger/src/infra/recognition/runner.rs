//! `RecognitionRunner` — the ASC 606 S6 **release** mechanism (design §4.3,
//! Group D1). It turns a due `PENDING` recognition segment into recognized
//! revenue by posting **one balanced entry** through the Slice 1
//! [`PostingService`]:
//!
//! | Line | Side | Account class |
//! |------|------|---------------|
//! | Recognize | DR | `CONTRACT_LIABILITY` (the segment's stream) |
//! | Revenue   | CR | `REVENUE` (the **same** stream) |
//!
//! `amount_minor` is the segment's amount, `currency` the schedule's currency,
//! and both legs carry the schedule's `revenue_stream` (per-stream disaggregation,
//! §4.5 — DR and CR draw the same stream). Each line's chart `account_id` is bound
//! from the provisioned chart of accounts via [`load_chart`] + [`ChartIndex::resolve`]
//! (mirroring [the invoice-post bind path](crate::infra::invoice_post)); the
//! per-line scale is resolved from the currency registry, exactly as the
//! settlement / invoice posts.
//!
//! **Atomicity + at-most-once.** The post threads a
//! [`RecognitionStampSidecar`](crate::infra::recognition::sidecar::RecognitionStampSidecar)
//! so the journal entry, the `recognized_minor += amount` counter bump (under the
//! per-schedule over-recognition cap CHECK → [`DomainError::OverRecognition`] 409),
//! and the segment `→ DONE` stamp all commit in the SAME serializable transaction
//! or roll back together (§4.3). The entry's `source_doc_type = RECOGNITION` +
//! `source_business_id = "{schedule_id}:{segment_no}"` key the Slice 1
//! `IdempotencyGate`, so the release is at-most-once per
//! `(tenant, RECOGNITION, schedule_id:segment_no)` — a replay returns the prior
//! [`PostingRef`] without re-crediting (and the sidecar never runs on a replay).
//!
//! **Scope.** [`Self::run_period`] releases the due `PENDING` segments for a
//! `(tenant, period_id)` in ascending `(schedule_id, segment_no)` order, applying
//! the E1 out-of-order → `QUEUED` guard, the E3 missed-close reassignment, and
//! the E4 obligation gate. The single-active-run orchestration + `recognition_run`
//! row live in the [`RecognitionRunService`](super::run_service). Group F adds:
//! [`Self::release_reversal`] (the `DR Revenue / CR CL` clawback keyed
//! `schedule_id:segment_no:reversal`, decrementing `recognized_minor`); the §9
//! recognition metrics (recognized-minor on release, queue-depth on a park); and
//! the EXPLICIT `RECOGNITION_PERIOD_QUEUED` (a park) + `RECOGNITION_DOUBLE_CREDIT`
//! (a detected re-credit) alarms — the `OVER_RECOGNITION` alarm is the posting
//! engine's (it fires on the rolled-back release). `effective_at` is the first
//! day of the segment's own `period_id` month (the natural-period convention);
//! the OPEN-period gate is the foundation's (the post fails `PeriodClosed` if the
//! segment's period is not open).

use std::sync::Arc;

use bss_ledger_sdk::{
    AccountClass, MappingStatus, PostEntry, PostLine, PostingRef, Side, SourceDocType,
};
use chrono::NaiveDate;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{NewEntry, NewLine};
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::domain::ports::obligation_state::{
    AlwaysSatisfiedObligationState, ObligationContext, ObligationStateResolver,
};
use crate::domain::status::SEGMENT_STATUS_DONE;
use crate::infra::currency_scale::CurrencyScaleResolver;
use crate::infra::events::payloads::{
    AffectedItem, AlarmCategory, AlarmSeverity, LedgerInvariantAlarm,
};
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::posting::chart::{ChartIndex, load_chart};
use crate::infra::posting::service::{PostSidecar, PostingService};
use crate::infra::recognition::sidecar::{RecognitionReversalSidecar, RecognitionStampSidecar};
use crate::infra::storage::repo::recognition_repo::DuePendingSegment;
use crate::infra::storage::repo::{RecognitionRepo, ReferenceRepo};
use time::OffsetDateTime;

/// Origin literal stamped on posts made through this service (mirrors the
/// invoice-post / settlement orchestrators).
const ORIGIN_SYSTEM: &str = "SYSTEM";

/// A single segment to release: the schedule/segment identity + the stream /
/// currency / amount the `DR CL / CR Revenue` entry posts with. Built from a
/// [`DuePendingSegment`] read, or supplied directly by a caller that already
/// holds the context (the Group E orchestrator / the Group F job).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleasableSegment {
    pub schedule_id: String,
    pub segment_no: i32,
    pub period_id: String,
    pub amount_minor: i64,
    pub revenue_stream: String,
    pub currency: String,
}

impl From<DuePendingSegment> for ReleasableSegment {
    fn from(s: DuePendingSegment) -> Self {
        Self {
            schedule_id: s.schedule_id,
            segment_no: s.segment_no,
            period_id: s.period_id,
            amount_minor: s.amount_minor,
            revenue_stream: s.revenue_stream,
            currency: s.currency,
        }
    }
}

/// The outcome of releasing one segment: the segment's id + the resulting
/// [`PostingRef`] (`replayed = true` when the release was an idempotent replay of
/// a prior run — at-most-once held).
#[derive(Clone, Debug)]
pub struct ReleasedSegment {
    pub schedule_id: String,
    pub segment_no: i32,
    pub posting: PostingRef,
}

/// A small summary of a `run_period` pass: how many due segments were released
/// (fresh + replayed) and the per-segment posting refs. The ordering-gap /
/// QUEUED accounting is Group E; for Group D this just tallies the releases.
#[derive(Clone, Debug, Default)]
pub struct RunPeriodSummary {
    /// Segments released on THIS pass (a fresh post, `replayed = false`).
    pub released: usize,
    /// Segments that were already released (an idempotent `RECOGNITION` replay).
    pub replayed: usize,
    /// Segments parked `QUEUED` this pass (E1 ordering — a lower-period
    /// predecessor was not yet `DONE`, so the segment is delayed, not released).
    pub queued: usize,
    /// Segments skipped this pass by the E4 obligation gate (the obligation was
    /// not satisfied — delayed, never released early). v1 never skips (the
    /// default resolver always proceeds).
    pub skipped: usize,
    /// The per-segment release outcomes, in release order.
    pub segments: Vec<ReleasedSegment>,
}

/// Releases due recognition segments through the Slice 1 posting engine. Holds
/// only what it needs: the chart reader ([`ReferenceRepo`]), the per-line scale
/// resolver, the [`PostingService`], and the [`RecognitionRepo`] (the due-segment
/// read + the in-txn counter/stamp writes the sidecar drives).
pub struct RecognitionRunner {
    posting: PostingService,
    reference: ReferenceRepo,
    resolver: CurrencyScaleResolver,
    recognition: RecognitionRepo,
    /// E4 run-gating: consulted per segment before release (a NOT-satisfied
    /// obligation delays the release). v1 default = always-satisfied (proceed).
    obligation: Arc<dyn ObligationStateResolver>,
    /// Metrics sink (design §9): the recognized-minor counter on each release,
    /// the over-recognition + double-credit counters + the queue-depth gauge on
    /// their paths, and the run-duration histogram (emitted by the run-service).
    /// Held behind the port so unit tests pass [`NoopLedgerMetrics`].
    metrics: Arc<dyn LedgerMetricsPort>,
    /// Publisher for the out-of-band recognition alarms the runner raises
    /// EXPLICITLY (the `RECOGNITION_PERIOD_QUEUED` park + the
    /// `RECOGNITION_DOUBLE_CREDIT` stamp breach). The `OVER_RECOGNITION` alarm is
    /// raised by the posting engine's `alarm_for` (it fires on the rolled-back
    /// release), so the runner does not double-emit it.
    publisher: Arc<LedgerEventPublisher>,
}

impl RecognitionRunner {
    /// Build the runner over one database provider + the event publisher +
    /// the metrics sink (threaded into the posting engine + the §9 recognition
    /// metrics). Mirrors
    /// [`crate::infra::invoice_post::InvoicePostService::new`] /
    /// [`crate::infra::payment::settle::SettlementService::new`].
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let posting = PostingService::new(db.clone(), Arc::clone(&publisher));
        let reference = ReferenceRepo::new(db.clone());
        let resolver = CurrencyScaleResolver::new(ReferenceRepo::new(db.clone()));
        let recognition = RecognitionRepo::new(db);
        Self {
            posting,
            reference,
            resolver,
            recognition,
            // v1: no Subscriptions feed — proceed (design §4.3). The real
            // fail-safe reader replaces this when the feed lands (Slice 7).
            obligation: Arc::new(AlwaysSatisfiedObligationState),
            metrics,
            publisher,
        }
    }

    /// Release the due `PENDING` segments for `(tenant, period_id)` in ascending
    /// `(schedule_id, segment_no)` order (Group D — the ordering GAP guard that a
    /// predecessor segment be `DONE` is Group E). Each segment is released via
    /// [`Self::release_segment`]; a release error short-circuits the pass and
    /// propagates (the segments already released stay committed — each is its own
    /// atomic post). Returns a [`RunPeriodSummary`] tallying fresh vs replayed
    /// releases + the per-segment refs.
    ///
    /// `run_id` labels every release of this pass on its segment row + the posted
    /// entry's audit linkage; the caller mints it (the Group E orchestration owns
    /// the `recognition_run` row + the single-active-run lock — Group D just
    /// threads the id through).
    ///
    /// # Errors
    /// Any [`DomainError`] a per-segment release raises ([`DomainError::OverRecognition`],
    /// [`DomainError::PeriodClosed`], [`DomainError::AccountClosed`], …) or
    /// [`DomainError::Internal`] on an infrastructure fault.
    pub async fn run_period(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        run_id: Uuid,
    ) -> Result<RunPeriodSummary, DomainError> {
        // TODO(slice-7): `list_due_pending_segments` is an unbounded feed — a
        // per-pass cap needs continuation semantics (resume cursor) so a
        // pathological backlog can't load an unbounded set into one pass; out of
        // scope here.
        let due = self
            .recognition
            .list_due_pending_segments(scope, tenant, period_id)
            .await
            .map_err(|e| DomainError::Internal(format!("list due segments: {e}")))?;

        // E3 missed-close (§4.3 E-2): a segment whose own target period has
        // CLOSED posts into the tenant's current open period instead (its target
        // stays on the segment row for audit). Resolved once per pass.
        let current_open = self
            .recognition
            .current_open_period(scope, tenant)
            .await
            .map_err(|e| DomainError::Internal(format!("current open period: {e}")))?;

        // Load the chart of accounts ONCE per pass (tenant-scoped + stable across
        // the pass): every segment binds its `DR CL / CR REVENUE` legs from the
        // same chart, so a per-segment `load_chart` (a full chart scan) would be a
        // needless N+1 against an immutable-within-the-pass projection. Bind it by
        // reference into each release.
        let chart = load_chart(&self.reference, scope, tenant).await?;

        let mut summary = RunPeriodSummary::default();
        for seg in due {
            let seg: ReleasableSegment = seg.into();

            // E4 run-gating: release only when the performance obligation is
            // satisfied — never early. A NOT-satisfied obligation delays the
            // segment (left PENDING; a later run retries). v1 default proceeds.
            let obligation = ObligationContext {
                tenant_id: tenant,
                schedule_id: seg.schedule_id.clone(),
                subscription_ref: None,
            };
            if !self.obligation.is_satisfied(&obligation).await {
                summary.skipped += 1;
                continue;
            }

            // E1 ordering (§4.6): a lower-`period_id` predecessor of the SAME
            // schedule that is not yet `DONE` ⇒ park this segment `QUEUED` (do
            // NOT release out of order); a later run drains it once the
            // predecessor commits.
            let undone = self
                .recognition
                .count_predecessors_not_done(scope, tenant, &seg.schedule_id, &seg.period_id)
                .await
                .map_err(|e| DomainError::Internal(format!("predecessor check: {e}")))?;
            if undone > 0 {
                self.recognition
                    .mark_segment_queued(scope, tenant, &seg.schedule_id, seg.segment_no)
                    .await
                    .map_err(|e| DomainError::Internal(format!("mark queued: {e}")))?;
                summary.queued += 1;
                // F3: one Warn `RECOGNITION_PERIOD_QUEUED` alarm per parked segment
                // (best-effort, out-of-band) — a later run drains it once the
                // predecessor commits, so it is re-detected until the gap closes.
                self.emit_period_queued(
                    ctx,
                    tenant,
                    &seg.schedule_id,
                    seg.segment_no,
                    &seg.period_id,
                )
                .await;
                continue;
            }

            // E3 missed-close: if the segment's own target period is closed
            // (strictly before the current open period), release into the current
            // open period — the segment row keeps its original `period_id` as the
            // audit target. Otherwise release into the segment's own period. (The
            // "do not release a FUTURE period early" bound is the caller's: the
            // Group F job targets the CURRENT period, so `list_due_pending_segments`
            // — `period_id <= target` — never enumerates a future segment. See
            // `RecognitionRunJob` / H1.)
            let release_seg = match &current_open {
                Some(open) if seg.period_id.as_str() < open.as_str() => ReleasableSegment {
                    period_id: open.clone(),
                    ..seg.clone()
                },
                _ => seg.clone(),
            };
            let posting = match self
                .release_segment_with_chart(ctx, scope, tenant, &chart, &release_seg, run_id)
                .await
            {
                Ok(posting) => posting,
                Err(e) => {
                    // §9 `ledger_over_recognition_total`: count the per-schedule cap
                    // breach here (the `OVER_RECOGNITION` alarm is the posting
                    // engine's `alarm_for`; this is the dedicated counter). Other
                    // rejections are not over-recognition.
                    if matches!(e, DomainError::OverRecognition(_)) {
                        self.metrics.over_recognition();
                    }
                    return Err(e);
                }
            };
            if posting.replayed {
                summary.replayed += 1;
            } else {
                summary.released += 1;
            }
            summary.segments.push(ReleasedSegment {
                schedule_id: seg.schedule_id,
                segment_no: seg.segment_no,
                posting,
            });
        }
        // F3: observe the segments this pass parked QUEUED out-of-order as the
        // recognition-period queue-depth gauge (design §9). NOTE: this records
        // THIS pass's parked count, not the standing backlog, and
        // the gauge is unlabelled — so concurrent passes (the per-tenant ticker + a
        // REST trigger, across tenants) clobber each other's last write. It signals
        // "a pass just parked N", not "N are queued now". An accurate backlog gauge
        // needs a per-tenant label + a `COUNT(status='QUEUED')` read (follow-up).
        self.metrics
            .recognition_period_queue_depth(i64::try_from(summary.queued).unwrap_or(i64::MAX));
        Ok(summary)
    }

    /// Release ONE due segment: build the balanced `DR CONTRACT_LIABILITY /
    /// CR REVENUE` entry (both on the segment's stream, the schedule's currency,
    /// `amount_minor = segment.amount_minor`), bind each leg's chart `account_id`,
    /// resolve per-line scale, and post through [`PostingService`] threading the
    /// [`RecognitionStampSidecar`] (the `recognized_minor` delta + segment `DONE`
    /// stamp commit in the same txn). Idempotent on
    /// `(tenant, RECOGNITION, schedule_id:segment_no)` — a replay returns the prior
    /// [`PostingRef`] (`replayed = true`) without re-crediting.
    ///
    /// # Errors
    /// [`DomainError::OverRecognition`] when the per-schedule cap CHECK rejects the
    /// release; [`DomainError::AccountClosed`] when the stream's `CONTRACT_LIABILITY`
    /// / `REVENUE` account is not provisioned; any foundation rejection
    /// (period-closed / negative-balance / …) or [`DomainError::Internal`] on an
    /// infrastructure fault.
    pub async fn release_segment(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        segment: &ReleasableSegment,
        run_id: Uuid,
    ) -> Result<PostingRef, DomainError> {
        // Standalone release (a single-segment caller / a test): load the chart
        // once, then delegate to the chart-bound path. A `run_period` pass loads
        // the chart ONCE for the whole pass and calls
        // [`Self::release_segment_with_chart`] directly (no per-segment scan).
        let chart = load_chart(&self.reference, scope, tenant).await?;
        self.release_segment_with_chart(ctx, scope, tenant, &chart, segment, run_id)
            .await
    }

    /// Release ONE due segment against an ALREADY-LOADED chart of accounts — the
    /// pass-internal release path. Identical to [`Self::release_segment`] but
    /// takes the tenant chart by reference (hoisted once per `run_period` pass,
    /// stable across it) instead of scanning the chart per segment.
    ///
    /// # Errors
    /// Same as [`Self::release_segment`].
    async fn release_segment_with_chart(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        chart: &ChartIndex,
        segment: &ReleasableSegment,
        run_id: Uuid,
    ) -> Result<PostingRef, DomainError> {
        // Build the balanced two-line entry (nil placeholder account_ids; bound
        // below from the chart, like every other post path).
        let entry = build_recognition_entry(ctx, tenant, segment);

        // Bind each leg's chart account_id (per-stream CONTRACT_LIABILITY / REVENUE
        // resolve on the segment's stream) from the passed-in chart.
        let mut bound = entry;
        for line in &mut bound.lines {
            line.account_id = resolve_line(chart, line).ok_or_else(|| {
                DomainError::AccountClosed(format!(
                    "no provisioned account for class {} / stream {:?} / currency {}",
                    line.account_class.as_str(),
                    line.revenue_stream,
                    line.currency
                ))
            })?;
        }

        // Map to the engine's NewEntry/NewLine (resolving per-line scale) + post,
        // threading the stamp sidecar so the counter bump + segment DONE stamp
        // commit atomically with the entry.
        let sidecar: Arc<dyn PostSidecar> = Arc::new(RecognitionStampSidecar {
            tenant_id: tenant,
            schedule_id: segment.schedule_id.clone(),
            segment_no: segment.segment_no,
            // The entry's period — the segment's own, or the current-open period
            // when E-2 reassigned it (the caller passes the reassigned segment).
            period_id: segment.period_id.clone(),
            amount_minor: segment.amount_minor,
            revenue_stream: segment.revenue_stream.clone(),
            currency: segment.currency.clone(),
            run_id,
            // The runner holds the publisher + ctx; thread them in so the
            // `revenue.recognized` event publishes in the SAME release txn.
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });
        let posting = match self.post_bound(ctx, scope, bound, sidecar).await {
            Ok(posting) => posting,
            Err(err) => {
                // F3 (best-effort): a concurrent run that already released this
                // segment leaves the per-segment `RECOGNITION` claim present, so
                // the loser normally returns an idempotent replay BEFORE the
                // sidecar — no double-credit. If instead the loser reached the
                // sidecar and its `stamp_segment_done` matched no PENDING/QUEUED
                // row (the segment is already `DONE`), that is a detected
                // double-credit attempt: raise the `RECOGNITION_DOUBLE_CREDIT`
                // alarm + counter. The original error still propagates (the post
                // rolled back — no second credit landed).
                self.detect_double_credit(ctx, scope, tenant, segment, &err)
                    .await;
                return Err(err);
            }
        };
        // F3: count the recognized revenue moved CONTRACT_LIABILITY → REVENUE on a
        // FRESH release (a replay re-credits nothing, so it is not counted). The
        // stream label is the schedule's revenue stream (design §9).
        if !posting.replayed {
            self.metrics
                .revenue_recognized_minor(segment.amount_minor, &segment.revenue_stream);
        }
        Ok(posting)
    }

    /// Reverse / claw back ONE already-released segment (design §4.3, Group F1):
    /// post the compensating `DR REVENUE / CR CONTRACT_LIABILITY` entry (the
    /// mirror of [`Self::release_segment`] — same stream / currency / amount as
    /// the original release) through [`PostingService`], threading the
    /// [`RecognitionReversalSidecar`] so the `recognized_minor -= amount`
    /// decrement commits in the SAME txn. Idempotent on
    /// `(tenant, RECOGNITION, schedule_id:segment_no:reversal)` — a replay returns
    /// the prior [`PostingRef`] without re-reversing. **The reversed segment stays
    /// `DONE`** (its release happened and was compensated; re-recognizing the
    /// period needs a new schedule version, Phase 3) — this method does NOT touch
    /// the `recognition_segment` row.
    ///
    /// No REST endpoint in v1: a reversal is invoked by the Phase 3
    /// schedule-change / correction path (or a maintenance caller); this is the
    /// mechanism it builds on.
    ///
    /// # Errors
    /// [`DomainError::OverRecognition`] when the decrement would drive
    /// `recognized_minor` below zero (a reversal larger than the cumulative
    /// recognized — the non-negative cap CHECK rejects it);
    /// [`DomainError::AccountClosed`] when the stream's `REVENUE` /
    /// `CONTRACT_LIABILITY` account is not provisioned; any foundation rejection
    /// (period-closed / …) or [`DomainError::Internal`] on an infra fault.
    pub async fn release_reversal(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        segment: &ReleasableSegment,
    ) -> Result<PostingRef, DomainError> {
        // Build the balanced reversing entry (DR REVENUE / CR CONTRACT_LIABILITY —
        // opposite of the release), keyed `schedule_id:segment_no:reversal`.
        let entry = build_reversal_entry(ctx, tenant, segment);

        // Bind each leg's chart account_id (same per-stream classes as the
        // release, just opposite sides).
        let chart = load_chart(&self.reference, scope, tenant).await?;
        let mut bound = entry;
        for line in &mut bound.lines {
            line.account_id = resolve_line(&chart, line).ok_or_else(|| {
                DomainError::AccountClosed(format!(
                    "no provisioned account for class {} / stream {:?} / currency {}",
                    line.account_class.as_str(),
                    line.revenue_stream,
                    line.currency
                ))
            })?;
        }

        // Thread the reversal sidecar: it DECREMENTS `recognized_minor` (negative
        // delta, under the non-negative cap CHECK) and leaves the segment row
        // untouched (`status = DONE` stays).
        let sidecar: Arc<dyn PostSidecar> = Arc::new(RecognitionReversalSidecar {
            tenant_id: tenant,
            schedule_id: segment.schedule_id.clone(),
            segment_no: segment.segment_no,
            period_id: segment.period_id.clone(),
            amount_minor: segment.amount_minor,
            revenue_stream: segment.revenue_stream.clone(),
            currency: segment.currency.clone(),
            // The runner holds the publisher + ctx; thread them in so the
            // `revenue.recognition_reversed` event publishes in the SAME reversal
            // txn.
            publisher: Arc::clone(&self.publisher),
            ctx: ctx.clone(),
        });
        self.post_bound(ctx, scope, bound, sidecar).await
    }

    /// Best-effort `RECOGNITION_DOUBLE_CREDIT` detection on a failed release: if
    /// the failed segment is now `DONE` (a concurrent run already released it),
    /// raise the alarm + counter. Re-reads the segment once (scoped); a read
    /// failure is swallowed (this is a best-effort diagnostic on an already-failed
    /// release, never a second error). Only the `stamp_segment_done` invariant
    /// breach (an `Internal` error) is a double-credit candidate — an
    /// `OverRecognition` / `AccountClosed` / period rejection is unrelated, so
    /// those skip the probe. (Do NOT widen the probe to
    /// `OverRecognition`. A same-segment second credit is caught by the per-segment
    /// `RECOGNITION` idempotency claim BEFORE the post, so an `OverRecognition` cap
    /// trip is a cross-segment over-recognition — not a double-credit; alarming it
    /// `RECOGNITION_DOUBLE_CREDIT` would be a false positive.)
    async fn detect_double_credit(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        segment: &ReleasableSegment,
        err: &DomainError,
    ) {
        if !matches!(err, DomainError::Internal(_)) {
            return;
        }
        let already_done = self
            .recognition
            .list_segments(scope, tenant, &segment.schedule_id)
            .await
            .ok()
            .into_iter()
            .flatten()
            .any(|s| s.segment_no == segment.segment_no && s.status == SEGMENT_STATUS_DONE);
        if !already_done {
            return;
        }
        self.metrics.recognition_double_credit();
        let code = AlarmCategory::RecognitionDoubleCredit.as_str().to_owned();
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::RecognitionDoubleCredit,
            severity: AlarmSeverity::Critical,
            tenant_id: tenant,
            scope: format!(
                "tenant:{tenant}/flow:RECOGNITION/business:{}:{}",
                segment.schedule_id, segment.segment_no
            ),
            code,
            detail: format!(
                "second credit attempted for an already-DONE segment \
                 (schedule={}, segment={})",
                segment.schedule_id, segment.segment_no
            ),
            affected: vec![AffectedItem {
                id: format!("{}:{}", segment.schedule_id, segment.segment_no),
                currency: segment.currency.clone(),
                expected_minor: 0,
                actual_minor: segment.amount_minor,
            }],
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;
    }

    /// Emit one out-of-band `RECOGNITION_PERIOD_QUEUED` `Warn` alarm for a segment
    /// parked out-of-order (design §4.6 / §6). Fire-and-forget; the run continues.
    async fn emit_period_queued(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        schedule_id: &str,
        segment_no: i32,
        period_id: &str,
    ) {
        let code = AlarmCategory::RecognitionPeriodQueued.as_str().to_owned();
        let alarm = LedgerInvariantAlarm {
            category: AlarmCategory::RecognitionPeriodQueued,
            severity: AlarmSeverity::Warn,
            tenant_id: tenant,
            scope: format!("tenant:{tenant}/flow:RECOGNITION/business:{schedule_id}:{segment_no}"),
            code,
            detail: format!(
                "segment parked QUEUED out-of-order (schedule={schedule_id}, \
                 segment={segment_no}, period={period_id}): a lower-period \
                 predecessor is not yet DONE"
            ),
            affected: Vec::new(),
        };
        self.publisher.emit_invariant_alarm(ctx, alarm).await;
    }

    /// Map an already-account-bound recognition [`PostEntry`] to the engine's
    /// `NewEntry`/`NewLine` (resolving each line's scale) and post with the stamp
    /// sidecar. Mirrors the settlement orchestrator's `post_bound`.
    async fn post_bound(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        entry: PostEntry,
        sidecar: Arc<dyn PostSidecar>,
    ) -> Result<PostingRef, DomainError> {
        let new_entry = NewEntry {
            entry_id: entry.entry_id,
            tenant_id: entry.tenant_id,
            // v1: one legal entity per tenant — derived server-side.
            legal_entity_id: entry.tenant_id,
            period_id: entry.period_id.clone(),
            entry_currency: entry.entry_currency.clone(),
            source_doc_type: entry.source_doc_type,
            source_business_id: entry.source_business_id.clone(),
            reverses_entry_id: entry.reverses_entry_id,
            reverses_period_id: entry.reverses_period_id.clone(),
            posted_at_utc: OffsetDateTime::now_utc(),
            effective_at: entry.effective_at,
            origin: ORIGIN_SYSTEM.to_owned(),
            posted_by_actor_id: entry.posted_by_actor_id,
            correlation_id: entry.correlation_id,
            rounding_evidence: serde_json::Value::Null,
            // Slice 5: recognition is translation, not a re-lock (S6 does NOT
            // re-lock, spec §3.2); the schedule currency is as posted. None here.
            rate_snapshot_ref: None,
        };
        let mut new_lines: Vec<NewLine> = Vec::with_capacity(entry.lines.len());
        for line in entry.lines {
            let scale = self
                .resolver
                .resolve(scope, entry.tenant_id, &line.currency)
                .await
                .map_err(|e| DomainError::Internal(format!("currency scale resolve: {e}")))?;
            new_lines.push(new_line(line, scale));
        }
        self.posting
            .post(ctx, scope, new_entry, new_lines, Some(sidecar))
            .await
    }
}

/// The `RECOGNITION` idempotency business id for one released segment:
/// `"{schedule_id}:{segment_no}"` (design §4.1 / §7). Set as the entry's
/// `source_business_id`; with `source_doc_type = RECOGNITION` it keys the Slice 1
/// `IdempotencyGate` at-most-once per `(tenant, RECOGNITION, schedule_id:segment_no)`.
#[must_use]
fn recognition_business_id(schedule_id: &str, segment_no: i32) -> String {
    format!("{schedule_id}:{segment_no}")
}

/// The `RECOGNITION` idempotency business id for one segment **reversal**:
/// `"{schedule_id}:{segment_no}:reversal"` (design §4.3). Distinct from the
/// forward-release key (`schedule_id:segment_no`), so a reversal is its own
/// at-most-once unit and can never collide with the original `DONE` release.
#[must_use]
fn reversal_business_id(schedule_id: &str, segment_no: i32) -> String {
    format!("{schedule_id}:{segment_no}:reversal")
}

/// Build the balanced `DR CONTRACT_LIABILITY / CR REVENUE` [`PostEntry`] for one
/// segment release. Both legs carry the segment's `revenue_stream` (per-stream
/// disaggregation, §4.5) and the schedule's `currency`; the amounts are equal
/// (`amount_minor`), so `Σ DR == Σ CR` exactly. Account ids are nil placeholders
/// (bound from the chart by the caller). The `effective_at` is the first day of
/// the segment's `period_id` month (Group D natural-period convention; the
/// OPEN-period gate + E-2 reassignment are the foundation's / Group E's).
fn build_recognition_entry(
    ctx: &SecurityContext,
    tenant: Uuid,
    segment: &ReleasableSegment,
) -> PostEntry {
    let effective_at = first_day_of_period(&segment.period_id);
    let dr = recognition_line(segment, AccountClass::ContractLiability, Side::Debit);
    let cr = recognition_line(segment, AccountClass::Revenue, Side::Credit);
    PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        period_id: segment.period_id.clone(),
        entry_currency: segment.currency.clone(),
        source_doc_type: SourceDocType::Recognition,
        source_business_id: recognition_business_id(&segment.schedule_id, segment.segment_no),
        effective_at,
        posted_by_actor_id: ctx.subject_id(),
        correlation_id: Uuid::now_v7(),
        reverses_entry_id: None,
        reverses_period_id: None,
        lines: vec![dr, cr],
    }
}

/// Build the balanced **reversal** `DR REVENUE / CR CONTRACT_LIABILITY`
/// [`PostEntry`] for one segment clawback (design §4.3) — the mirror of
/// [`build_recognition_entry`]: the SAME stream both legs, the schedule's
/// currency, equal amounts (`amount_minor`), so `Σ DR == Σ CR` exactly. Keyed
/// `schedule_id:segment_no:reversal` under `RECOGNITION`. Account ids are nil
/// placeholders (bound from the chart by the caller). The `effective_at` is the
/// first day of the segment's `period_id` month (the same natural-period
/// convention as the release; the OPEN-period gate is the foundation's).
fn build_reversal_entry(
    ctx: &SecurityContext,
    tenant: Uuid,
    segment: &ReleasableSegment,
) -> PostEntry {
    let effective_at = first_day_of_period(&segment.period_id);
    // Opposite sides of the release: DR REVENUE (give back the recognized
    // revenue) / CR CONTRACT_LIABILITY (restore the deferred balance).
    let dr = recognition_line(segment, AccountClass::Revenue, Side::Debit);
    let cr = recognition_line(segment, AccountClass::ContractLiability, Side::Credit);
    PostEntry {
        entry_id: Uuid::now_v7(),
        tenant_id: tenant,
        period_id: segment.period_id.clone(),
        entry_currency: segment.currency.clone(),
        source_doc_type: SourceDocType::Recognition,
        source_business_id: reversal_business_id(&segment.schedule_id, segment.segment_no),
        effective_at,
        posted_by_actor_id: ctx.subject_id(),
        correlation_id: Uuid::now_v7(),
        reverses_entry_id: None,
        reverses_period_id: None,
        lines: vec![dr, cr],
    }
}

/// Build one recognition [`PostLine`] for `class`/`side` from the segment: the
/// stream-tagged per-stream class (`CONTRACT_LIABILITY` / `REVENUE`), the
/// schedule's currency, the segment amount. The `account_id` is a nil placeholder
/// (bound from the chart by the caller). Recognition lines carry no
/// payer/invoice/tax dims — they move deferred revenue to earned revenue within
/// the seller's own ledger — so `payer_tenant_id` is the nil placeholder
/// ([`segment_payer_placeholder`]; both legs share it, so the entry is trivially
/// single-payer) and the optional dims are `None`.
fn recognition_line(segment: &ReleasableSegment, class: AccountClass, side: Side) -> PostLine {
    PostLine {
        line_id: Uuid::now_v7(),
        payer_tenant_id: segment_payer_placeholder(),
        seller_tenant_id: None,
        resource_tenant_id: None,
        account_id: Uuid::nil(),
        account_class: class,
        gl_code: None,
        side,
        amount_minor: segment.amount_minor,
        currency: segment.currency.clone(),
        invoice_id: None,
        due_date: None,
        revenue_stream: Some(segment.revenue_stream.clone()),
        mapping_status: MappingStatus::Resolved,
        functional_amount_minor: None,
        functional_currency: None,
        tax_jurisdiction: None,
        tax_filing_period: None,
        tax_rate_ref: None,
        invoice_item_ref: None,
        sku_or_plan_ref: None,
        price_id: None,
        pricing_snapshot_ref: None,
        po_allocation_group: None,
        credit_grant_event_type: None,
        ar_status: None,
    }
}

/// The payer-tenant placeholder for a recognition line. A recognition entry moves
/// the seller's own deferred revenue to earned revenue (no buyer is party to it),
/// so there is no real payer; the foundation's single-payer-tenant entry
/// invariant still wants a value, so the nil UUID stands in (both legs share it,
/// so the entry is trivially single-payer). The payer-on-the-schedule
/// (`payer_tenant_id`) is the audit fact, recorded on the schedule, not re-stamped
/// on the recognition lines.
#[must_use]
fn segment_payer_placeholder() -> Uuid {
    Uuid::nil()
}

/// First day of a `YYYYMM` `period_id` as the entry's `effective_at`. A
/// malformed period (not a parseable `YYYYMM`) falls back to [`NaiveDate::MIN`],
/// which the foundation's OPEN-period gate rejects — a malformed segment never
/// silently posts to a wrong date. (Group D's natural-period convention; E-2
/// missed-close reassignment is Group E.)
#[must_use]
fn first_day_of_period(period_id: &str) -> NaiveDate {
    parse_period(period_id)
        .and_then(|(y, m)| NaiveDate::from_ymd_opt(y, m, 1))
        // Defensive only — the segment row's period is validated at
        // schedule-build (`period_id_plus`), so this never fires in practice;
        // `MIN` is a const (no panic) the OPEN-period gate rejects.
        .unwrap_or(NaiveDate::MIN)
}

/// Parse a `YYYYMM` period id into `(year, month)`; `None` when it is not a
/// 6-char string with a `1..=12` month (mirrors the validation in
/// [`crate::domain::period`]).
fn parse_period(period_id: &str) -> Option<(i32, u32)> {
    if period_id.len() != 6 {
        return None;
    }
    let year: i32 = period_id.get(0..4)?.parse().ok()?;
    let month: u32 = period_id.get(4..6)?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

/// Thin `PostLine` adapter over [`ChartIndex::resolve`]: per-stream classes
/// (`CONTRACT_LIABILITY` / `REVENUE`) key on the line's stream. Mirrors the
/// invoice-post / settlement `resolve_line`.
fn resolve_line(chart: &ChartIndex, line: &PostLine) -> Option<Uuid> {
    chart.resolve(
        line.account_class,
        &line.currency,
        line.revenue_stream.as_deref(),
    )
}

/// Map one SDK [`PostLine`] + its resolved scale to the engine's [`NewLine`]
/// (mirrors `invoice_post::new_line` / `settle::new_line`).
fn new_line(line: PostLine, scale: u8) -> NewLine {
    NewLine {
        line_id: line.line_id,
        payer_tenant_id: line.payer_tenant_id,
        seller_tenant_id: line.seller_tenant_id,
        resource_tenant_id: line.resource_tenant_id,
        account_id: line.account_id,
        account_class: line.account_class,
        gl_code: line.gl_code,
        side: line.side,
        amount_minor: line.amount_minor,
        currency: line.currency,
        currency_scale: scale,
        invoice_id: line.invoice_id,
        due_date: line.due_date,
        revenue_stream: line.revenue_stream,
        mapping_status: line.mapping_status,
        functional_amount_minor: line.functional_amount_minor,
        functional_currency: line.functional_currency,
        tax_jurisdiction: line.tax_jurisdiction,
        tax_filing_period: line.tax_filing_period,
        tax_rate_ref: line.tax_rate_ref,
        legal_entity_id: None,
        invoice_item_ref: line.invoice_item_ref,
        sku_or_plan_ref: line.sku_or_plan_ref,
        price_id: line.price_id,
        pricing_snapshot_ref: line.pricing_snapshot_ref,
        po_allocation_group: line.po_allocation_group,
        credit_grant_event_type: line.credit_grant_event_type,
        ar_status: line.ar_status,
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod runner_tests;
