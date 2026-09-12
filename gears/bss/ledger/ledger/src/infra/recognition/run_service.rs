//! `RecognitionRunService` — the Group E orchestration wrapper around a
//! [`RecognitionRunner`] pass (design §4.3 / §5, the S6 release). It brackets
//! one `run_period` pass with the `recognition_run` row lifecycle (dedup →
//! `RUNNING` → `DONE`/`FAILED`) and maps the [`RunPeriodSummary`] onto the SDK
//! [`RecognitionRunOutcome`] (`Ran` vs `Queued`).
//!
//! **Idempotent replay (dedup).** A trigger whose `(tenant, period_id, run_id)`
//! already has a `recognition_run` row replays that run reference
//! (`Ran { replayed: true }`) instead of starting a second run — the run-trigger
//! idempotency key (design §4.3). The key includes `period_id`, so reusing one
//! `run_id` across two periods runs both. A `None` `run_id` mints a fresh one
//! (`Uuid::now_v7`), so an un-keyed trigger always runs.
//!
//! **Single-active-run guard (`coord` lease).** A fresh trigger acquires a
//! [`coord`] lease keyed `recognition-run:{tenant}:{period_id}` (a TTL + renewal
//! heartbeat) before it runs. A concurrent trigger for the same
//! `(tenant, period_id)` sees [`CoordError::LeaseHeld`] and returns a no-op
//! replay (`Ran { replayed: true, released: 0 }`) instead of racing the holder.
//! This serialises redundant runs and removes the `SERIALIZABLE` contention two
//! overlapping passes would otherwise hit on the shared per-segment
//! `RECOGNITION` idempotency claim. Correctness never depended on it — each
//! segment release is already at-most-once via that claim + the `status = DONE`
//! guard — so a lease that lapses mid-pass (a pathologically long run) cannot
//! double-credit; it only re-admits a redundant pass. The heartbeat keeps the
//! lease live across a normal pass; a crashed run's lease lapses at TTL so the
//! period is re-runnable within ~a minute.
//!
//! **Run-row bracketing.** Under the held lease a fresh trigger inserts the row
//! `RUNNING`, runs the pass, then flips it `DONE` (success) or `FAILED` (a
//! release error is propagated after the `FAILED` flip — the segments already
//! released stay committed, each being its own atomic post).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bss_ledger_sdk::{RecognitionRunOutcome, RecognitionRunQueued, RecognitionRunRef};
use coord::{CoordError, LeaseGuard, LeaseManager};
use futures::FutureExt as _;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::ports::metrics::LedgerMetricsPort;
use crate::infra::events::publisher::LedgerEventPublisher;
use crate::infra::recognition::runner::RecognitionRunner;
use crate::infra::storage::repo::RecognitionRepo;
use time::OffsetDateTime;

/// Lease TTL for a recognition run. Comfortably longer than the renewal period
/// so one missed heartbeat (a transient DB blip) does not drop the lease, yet
/// short enough that a crashed run's lease lapses and the period is re-runnable
/// within ~a minute.
const RECOGNITION_LEASE_TTL: Duration = Duration::from_mins(1);
/// Renewal heartbeat period (~`TTL`/3): a live pass renews well before expiry.
const RECOGNITION_LEASE_RENEW: Duration = Duration::from_secs(20);

/// Orchestrates one recognition-run trigger: a single-active-run [`coord`] lease
/// bracketing the `recognition_run` row lifecycle (dedup → `RUNNING` →
/// `DONE`/`FAILED`) around a [`RecognitionRunner`] pass. Holds the runner (the
/// per-segment release engine), the [`RecognitionRepo`] (the run-row
/// reads/writes), and the lease manager — same `db`/`publisher` deps as the peer
/// payment services.
pub struct RecognitionRunService {
    runner: RecognitionRunner,
    recognition: RecognitionRepo,
    /// Single-active-run lease (design §4.3). Keyed
    /// `recognition-run:{tenant}:{period_id}`, built over the same `Db` as the
    /// repos. See [`coord`] + the module-level note.
    lease: LeaseManager,
    /// Metrics sink: the run-duration histogram is emitted here (it brackets the
    /// whole pass); the per-segment recognized-minor / over-recognition /
    /// double-credit / queue-depth metrics are emitted inside the runner.
    metrics: Arc<dyn LedgerMetricsPort>,
}

impl RecognitionRunService {
    /// Build the run-service over one database provider + the event publisher +
    /// the metrics sink (threaded into the runner's posting engine + the §9
    /// recognition metrics). The lease manager is built from the provider's
    /// `Db` (`db.db()`); mirrors how the peer payment services are built from a
    /// `db.clone()` + a publisher + a metrics clone.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        publisher: Arc<LedgerEventPublisher>,
        metrics: Arc<dyn LedgerMetricsPort>,
    ) -> Self {
        let lease = LeaseManager::new(db.db());
        let runner = RecognitionRunner::new(db.clone(), publisher, Arc::clone(&metrics));
        let recognition = RecognitionRepo::new(db);
        Self {
            runner,
            recognition,
            lease,
            metrics,
        }
    }

    /// Trigger a recognition run for `(tenant, period_id)`. Mints `run_id` when
    /// `None`; dedups on the `recognition_run` row (a prior row replays its
    /// reference without re-running); otherwise acquires the single-active-run
    /// lease and — under it — brackets a [`RecognitionRunner`] pass with the
    /// `RUNNING → DONE`/`FAILED` row lifecycle, mapping the
    /// [`RunPeriodSummary`](crate::infra::recognition::runner::RunPeriodSummary)
    /// onto the SDK outcome (`Queued` when any segment was parked out-of-order,
    /// else `Ran`). A peer already holding the lease yields a no-op replay.
    ///
    /// # Errors
    /// Any [`DomainError`] the underlying pass raises
    /// ([`DomainError::OverRecognition`], [`DomainError::PeriodClosed`],
    /// [`DomainError::AccountClosed`], …) — propagated AFTER the run row is
    /// flipped `FAILED`; or [`DomainError::Internal`] on a run-row read/write
    /// fault or a lease-acquire fault.
    pub async fn trigger(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        run_id: Option<Uuid>,
    ) -> Result<RecognitionRunOutcome, DomainError> {
        // Mint a fresh run_id for an un-keyed trigger; a caller-supplied one is
        // the idempotency key the dedup read below keys on.
        let run_id = run_id.unwrap_or_else(Uuid::now_v7);

        // Dedup: a trigger whose (tenant, period_id, run_id) already ran replays
        // that run reference (idempotent) instead of starting a second run. The
        // key includes period_id: a client that reuses one
        // run_id across two periods must run BOTH — keying on (tenant, run_id)
        // alone short-circuited the second period to the first and silently never
        // recognized it. The prior run's release tally is not re-derived — a replay
        // reports zero releases (the work already committed under the original run).
        if let Some(prior) = self
            .recognition
            .read_run(scope, tenant, period_id, run_id)
            .await
            .map_err(|e| DomainError::Internal(format!("read recognition_run: {e}")))?
        {
            return Ok(RecognitionRunOutcome::Ran(RecognitionRunRef {
                run_id,
                period_id: prior.period_id,
                replayed: true,
                released: 0,
                already_recognized: 0,
            }));
        }

        // Single-active-run guard: take the lease for this (tenant, period). A
        // concurrent run holding it ⇒ no-op replay (we do not start a second
        // pass for the same period); the holder releases its due segments.
        let lease_key = format!("recognition-run:{tenant}:{period_id}");
        let guard = match self.lease.acquire(&lease_key, RECOGNITION_LEASE_TTL).await {
            Ok(guard) => guard,
            Err(CoordError::LeaseHeld) => {
                return Ok(RecognitionRunOutcome::Ran(RecognitionRunRef {
                    run_id,
                    period_id: period_id.to_owned(),
                    replayed: true,
                    released: 0,
                    already_recognized: 0,
                }));
            }
            Err(e) => {
                return Err(DomainError::Internal(format!(
                    "recognition lease acquire ({lease_key}): {e}"
                )));
            }
        };

        // Keep the lease live across a long pass; stop the heartbeat before we
        // release, then free the slot (preserving the forensic `attempts` streak
        // on a failed run).
        let renewal = guard.spawn_renewal(RECOGNITION_LEASE_RENEW);
        let result = self.run_locked(ctx, scope, tenant, period_id, run_id).await;
        renewal.shutdown().await;
        Self::release_lease(guard, result.is_ok()).await;
        result
    }

    /// The bracketed pass under a held lease: insert the `RUNNING` row, run the
    /// `RecognitionRunner` pass (bracketed by the run-duration histogram), then
    /// flip the row `DONE` (success) or `FAILED` (error propagated after).
    async fn run_locked(
        &self,
        ctx: &SecurityContext,
        scope: &AccessScope,
        tenant: Uuid,
        period_id: &str,
        run_id: Uuid,
    ) -> Result<RecognitionRunOutcome, DomainError> {
        self.recognition
            .insert_run(scope, tenant, run_id, period_id, OffsetDateTime::now_utc())
            .await
            .map_err(|e| DomainError::Internal(format!("insert recognition_run: {e}")))?;

        // F3: bracket the release pass with the run-duration histogram (design §9),
        // recorded on every exit — success, error, AND panic (each consumed wall
        // time). The pass is wrapped in `catch_unwind`: a panic
        // would otherwise unwind past `finish_run`, leaving the row stuck `RUNNING`
        // so a same-key retry replays it as `released: 0`; instead the row flips
        // FAILED and the panic resumes.
        let started = Instant::now();
        let pass = std::panic::AssertUnwindSafe(
            self.runner
                .run_period(ctx, scope, tenant, period_id, run_id),
        )
        .catch_unwind()
        .await;
        self.metrics
            .recognition_run_duration(started.elapsed().as_secs_f64());
        let summary = match pass {
            Ok(Ok(summary)) => summary,
            Ok(Err(err)) => {
                self.flip_failed(scope, tenant, period_id, run_id).await;
                return Err(err);
            }
            Err(panic) => {
                self.flip_failed(scope, tenant, period_id, run_id).await;
                std::panic::resume_unwind(panic);
            }
        };

        self.recognition
            .finish_run(scope, tenant, period_id, run_id, true)
            .await
            .map_err(|e| DomainError::Internal(format!("finish recognition_run: {e}")))?;

        // Surface an obligation-stalled backlog: segments left
        // unreleased because a performance obligation was NOT satisfied are counted
        // in `summary.skipped` but were otherwise invisible. v1's obligation gate is
        // a stub that never skips, so this is dormant until the Slice 7 obligation
        // feed lands — at which point a non-zero count is an operator-actionable
        // stall, not a silent backlog.
        if summary.skipped > 0 {
            tracing::warn!(
                target: "bss.ledger.recognition",
                %period_id,
                skipped = summary.skipped,
                "recognition run left segments unreleased on an unsatisfied performance obligation"
            );
        }

        // Map the summary onto the SDK outcome: any out-of-order park (§4.6) ⇒
        // Queued (HTTP 202 `recognition-period-queued`); otherwise Ran (the run
        // released its due segments in order). `released` = fresh posts this
        // pass; `already_recognized` = idempotent RECOGNITION replays.
        let outcome = if summary.queued > 0 {
            RecognitionRunOutcome::Queued(RecognitionRunQueued {
                run_id,
                period_id: period_id.to_owned(),
                released: summary.released,
                queued: summary.queued,
            })
        } else {
            RecognitionRunOutcome::Ran(RecognitionRunRef {
                run_id,
                period_id: period_id.to_owned(),
                replayed: false,
                released: summary.released,
                already_recognized: summary.replayed,
            })
        };
        Ok(outcome)
    }

    /// Flip the run row `RUNNING → FAILED` (best-effort): a finish-row fault is
    /// logged, not propagated (it is subordinate to the original failure). Shared by
    /// the error and panic arms of the bracketed pass.
    async fn flip_failed(&self, scope: &AccessScope, tenant: Uuid, period_id: &str, run_id: Uuid) {
        if let Err(finish_err) = self
            .recognition
            .finish_run(scope, tenant, period_id, run_id, false)
            .await
        {
            tracing::error!(
                error = %finish_err,
                %run_id,
                "bss-ledger: failed to flip recognition_run FAILED after a run error/panic"
            );
        }
    }

    /// Release the run lease, logging (not failing) on a release fault — the run
    /// already committed, and a lingering lease lapses at its TTL. `succeeded`
    /// picks `release` (reset the `attempts` streak) vs `release_with_retry`
    /// (preserve it, so a flapping run stays visible to operators).
    async fn release_lease(guard: LeaseGuard, succeeded: bool) {
        let key = guard.key().to_owned();
        let released = if succeeded {
            guard.release().await
        } else {
            guard.release_with_retry().await
        };
        if let Err(e) = released {
            tracing::warn!(
                target: "bss-ledger",
                error = %e,
                lease_key = %key,
                "failed to release recognition-run lease (will lapse at TTL)"
            );
        }
    }
}
