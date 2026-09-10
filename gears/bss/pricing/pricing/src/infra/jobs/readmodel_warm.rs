//! `ReadModelWarmJob` — the sweep, and the whole of §4.4's degraded path.
//!
//! The reconciliation **is** the re-drive. A publish commit leaves a pending
//! `pricing_catalog_version_ref` and no version; this pass asks the registry
//! what version each pending handle landed in, hands the answer to the
//! projector, and arrives again next tick for whatever did not complete. There
//! is no second mechanism: §4.4's "the warm re-drive continues past the SLO
//! with no bound" is this pass running again.
//!
//! # Why a pull, and why here
//!
//! `CatalogVersionPublished` is the registry's event (D-66) and there is no
//! broker in this repository, so an inbound port would be a trait with neither
//! producer nor caller. The pull is already contracted:
//! `CatalogVersionRegistryV1::committed_version`'s own doc hands the overdue
//! decision to the caller "since only it knows how long the ref has been
//! outstanding" — and the only thing that knows that is a sweep over
//! `requested_at`. [`crate::infra::read_model`] carries the argument and names
//! the entry point a registry event handler will hook to when the registry gear
//! lands.
//!
//! # Inert without a registry, and that is a requirement rather than a nicety
//!
//! With `UnconfiguredCatalogVersionRegistryV1` every `committed_version` call
//! answers [`CatalogVersionRegistryError::Unconfigured`]. The pass detects that
//! **once** and returns at **debug** — no alarm, no error, never a boot or
//! shutdown failure. The sibling ledger's `RateSyncJob` states exactly this
//! posture for its own unconfigured provider, and the e2e that boots this gear
//! without a registry depends on it: a pass logging an error every five seconds
//! would make that boot unreadable.
//!
//! # What "incomplete" means here, and why the re-drive needs no second query
//!
//! The projector finalizes a ref and writes its subject's delta **warm in one
//! transaction**, so a committed ref always has a warm delta and the state
//! "committed but unwarm" is not reachable. A version is therefore incomplete
//! exactly while it still has a **pending** ref — which is what
//! [`catalog_version_ref_repo::list_pending_for_tenant`] returns. The re-drive is
//! consequently the same read as the first warm, and this pass needs no
//! "committed but incomplete" scan at all. **Reported**, because the plan this
//! group was built from expected two reads and the finalize's position
//! collapses them into one.
//!
//! # The scan is bounded per tenant, and the bound is a fact the pass acts on
//! (D-163)
//!
//! One tenant at a time: `pending_tenants` finds the tenants holding a pending
//! ref, and each is then read and resolved on its own. The bound has to be per
//! tenant rather than per pass, because on a cross-tenant page ordered by request
//! instant one tenant's stuck backlog fills the whole budget and what it defers
//! is every **other** tenant's completions — their frontiers do not advance, so
//! their publishes are unpinnable because someone else's are.
//!
//! Both bounds then feed [`PassCoverage`]: a tenant whose page **filled**, or one
//! for which the registry **errored** on any ref, gets no completion decision
//! this pass. The pass still resolves and warms everything it holds; what it does
//! not do is count a subject set it knows is partial. The frontier lags, which is
//! the safe direction, and `pin_eligibility_overdue` is what says so out loud.
//!
//! **Two numbers and an order, and they are the owner's** — D-163 hands them to
//! §F.2 rather than choosing them. `jobs.pending_refs_per_tenant` defaults to the
//! 500 this module used to spend cross-tenant;
//! `jobs.pending_tenants_per_pass` defaults to 250, and past it the tail of the
//! tenant order is not swept at all. The order is ascending tenant id and is not
//! rotating, which would need a cursor no table in §3.7 carries. **Reported.**
//!
//! # The two alarms, and the sink that does not exist
//!
//! `pricing.catalogversion.commit_overdue` (§3.6) and
//! `pricing.readmodel.pin_eligibility_overdue` (§4.4) are the two Critical
//! alarms the design set names **by string**, and both names are taken from it
//! rather than invented.
//!
//! **Both names are on the alarm plane** (D-238): `domain/ports/metrics.rs` and
//! `infra/metrics.rs` carry the same port-and-adapter pair the sibling ledger has,
//! with a `PricingAlarm` roster, and both firings go through the port as well as to
//! the log. The `tracing::error!` under the named string stays because it is what
//! the e2e boot without a registry reads; the counter is what an alerting rule can
//! see. These are the gear's only **Critical** alarms.
//!
//! # The degraded mark has no home, and none is invented
//!
//! §4.4 says completion "clears the degraded mark", and **no table in §3.7 has
//! a column for one**: `pricing_read_model` has none, and `pricing_operator_flag`
//! is forbidden from carrying version state by D-85. So the mark is **derived**
//! — a publish whose ref is still pending *is* the degraded state, and
//! completing it clears the state by making the predicate false. That is
//! precisely §4.4's own "no new event name is introduced; consumers observe
//! completion via the marker". No column, no flag. **Reported as a gap.**
//!
//! # The two signals measure two waits, and the instant that separates them
//! (D-166)
//!
//! `commit_observed_at` on the ref row is **when this gear first saw the
//! registry's answer** for that handle. The sweep stamps it the moment
//! `committed_version` answers `Ok(Some(_))`, in a statement of its own, before
//! and regardless of whether the projection then lands. That placement is the
//! whole mechanism: the projector writes a subject's delta warm in the **same
//! transaction that finalizes its ref**, so "committed but unwarm" is not a
//! state this store can hold and the observation has to be **recorded** rather
//! than derived from the ref's committed-ness.
//!
//! With it the two Critical signals stop overlapping:
//!
//! - **`PlanPublishDegraded`** — the ref is still unresolved, its commit **has**
//!   been observed, and that observation is older than §1.2's **5s** propagation
//!   SLO (`config.jobs.readmodel_degraded_after()`). This is the condition
//!   `fr-publish-fanout-atomicity` states and the batching wait is now outside
//!   it by construction rather than by hope.
//! - **`pricing.catalogversion.commit_overdue`** — the ref is pending, **not yet
//!   observed committed**, past the max batching-delay SLO
//!   (`config.jobs.catalog_version_overdue_after()`). The registry has not
//!   answered.
//!
//! The two are disjoint over one clock, which is the defect they were: an
//! earlier version of this module measured **both** against the batching SLO —
//! correctly for its own information, having no post-commit instant — so an
//! operator could not tell "the registry has not answered" from "the registry
//! answered and the warm is failing", the one distinction
//! `fr-publish-fanout-atomicity` exists to draw. Nothing goes silent in the
//! split: the four `(pending?, observed?, age)` states each land on exactly one
//! signal or none, and either failing state also holds the frontier and is
//! reported by `pin_eligibility_overdue`.
//!
//! `commit_observed_at` is an **upper bound** on the registry's commit — the
//! observation lags it by at most one sweep pass — so the degraded signal is
//! **late rather than false**, which is the safe direction for a Critical. The
//! accurate form is the registry supplying its own commit instant with the
//! version, and that is the registry gear's owed half of D-163's adoption, not
//! something this gear can approximate any closer.

use std::collections::BTreeMap;
use std::sync::Arc;

use bss_pricing_sdk::{CatalogVersion, PinFrontier};

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::JobsConfig;
use crate::domain::error::DomainError;
use crate::domain::events::CatalogEvent;
use crate::domain::ports::{CatalogVersionRegistryError, CatalogVersionRegistryV1};
use crate::domain::read_model::SubjectKind;
use crate::domain::scope_key::PlanId;
use crate::infra::jobs::unconvertible_threshold;
use crate::infra::read_model::{PassCoverage, ReadModelProjector};
use crate::infra::storage::repo::{
    NewOutboxEvent, PendingVersionRow, PlanPublishDegradedPayload, catalog_version_ref_repo,
    outbox_repo, pin_frontier_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// The `pricing_pin_frontier` rows one pass reads for the pin-eligibility
/// alarm. One row per tenant that has ever completed a version, so this bounds
/// an O(active tenants) read rather than an O(publishes) one.
const FRONTIER_SCAN_LIMIT: u64 = 1_000;

/// What one pass knows about a tenant's pin frontier when it comes to probe the
/// committed-version arm of pin-eligibility.
///
/// The cross-tenant scan at the head of the pass reads every frontier row, so on
/// the ordinary path the version is already in hand and re-reading it per tenant
/// is a second query for a row the pass holds. The distinction this type draws is the one a bare
/// `Option<CatalogVersion>` cannot: **"this tenant has no frontier row" and "the
/// scan that would have told us failed" are different facts**, and treating the
/// second as the first asks for the lowest committed version the tenant ever had
/// and raises a Critical on a healthy estate.
#[derive(Clone, Copy, Debug)]
enum Standing {
    /// The scan answered. `None` means the tenant genuinely has no frontier row.
    Read(Option<CatalogVersion>),
    /// The scan failed, so the probe reads this tenant's row for itself.
    Unknown,
}

/// The Critical alarm §3.6 names for a ref that stays `pending` past the max
/// batching-delay SLO. The string is the design set's, not this module's.
const ALARM_COMMIT_OVERDUE: &str = "pricing.catalogversion.commit_overdue";

/// The Critical alarm §4.4 names for a version that has not become pin-eligible
/// within the same SLO. Likewise the design set's string.
const ALARM_PIN_ELIGIBILITY_OVERDUE: &str = "pricing.readmodel.pin_eligibility_overdue";

/// What the registry answered about **one handle**.
///
/// Named rather than left as `Result<Option<CatalogVersion>, ()>`, because a
/// pass caches these per handle (D-234: a unit records every subject it projects
/// against one handle, so the rows repeat handles) and a cached `Err(())` reads
/// as a failure that has been swallowed rather than as the classification it is.
/// The unconfigured answer is deliberately **not** a member: it ends the pass
/// rather than describing a handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandleAnswer {
    /// The registry could not be asked. Costs the pass its right to judge any of
    /// this tenant's versions complete.
    Unresolved,
    /// Answered, and the handle has no version yet — the budgeted batching wait.
    NotYet,
    /// Answered with the version the handle landed in.
    Committed(CatalogVersion),
}

/// How long one cross-gear `committed_version` call may take before the pass
/// treats it as unanswered.
///
/// The bound on **one call**, and not a bound on the pass: a pass makes up to
/// `pending_tenants_per_pass x pending_refs_per_tenant` of these in sequence, so
/// three slow refs on one tenant already outlast a five-second tick.
///
/// **Nothing bounds the pass.** The tick can therefore lapse under a pass that is
/// still running and a peer start a second beside it, which for this sweep is
/// wasted work rather than a correctness hole — every write on the path is guarded
/// by a key or a predicate, as `module`'s warm ticker states.
/// [`ReadModelWarmJob::run`]'s *"Why the pass carries no deadline of its own"*
/// carries the argument against bounding it here.
/// The budget this pass makes its cross-gear call under.
///
/// [`registry_deadline::deployment_budget`] and not a constant of its own: the
/// operator's `registry_call_timeout_secs` is one knob for the gear's registry
/// calls, and a second number here means raising it for a slow registry leaves
/// this pass abandoning every ask at the old bound — so no pending ref resolves
/// and the read model stays empty with the knob apparently turned.
///
/// The **shape** stays local, though. `request_version_within` renders an
/// elapsed call as `CatalogVersionUnavailable`, which is right inside a write
/// transaction and wrong here: this pass treats "did not answer in time" as
/// `Unresolved`, exactly as it treats "answered with a failure".
fn registry_ask_timeout() -> std::time::Duration {
    crate::infra::registry_deadline::deployment_budget()
}

/// What the registry answered for one tenant's pending refs.
///
/// The two halves are one value because they are one fact about the pass: which
/// versions it may project, and whether it is entitled to judge any of them
/// complete (D-163 clause 2). Splitting them invites a caller to take the map and
/// forget the qualification, which is the defect.
#[derive(Default)]
struct Resolution {
    /// Refs grouped by the version the registry answered, ascending.
    by_version: BTreeMap<CatalogVersion, Vec<PendingVersionRow>>,
    /// At least one ref of this tenant could **not** be resolved this pass — a
    /// registry error, not an `Ok(None)`.
    any_unresolved: bool,
}

/// What one pass did. Returned rather than only logged so the suite can assert
/// the pass's behaviour instead of scraping its output.
///
/// # Why three `bool`s rather than a state enum
///
/// `clippy::struct_excessive_bools` is denied workspace-wide and its usual remedy
/// — replace the flags with an enum — is the wrong one here, because these are
/// **not** three states of one thing. They are three independent facts about one
/// pass, and a pass can hold any subset of them at once: the cross-tenant
/// frontier scan can fail while a degraded emit also fails while a per-tenant
/// block probe also fails. An enum would force a choice between them and lose
/// exactly what each field is for, which is telling an operator *which* signal
/// went missing. `inert` is a fourth and different kind again — a deployment
/// state, not an event.
///
/// The alternative shape, a nested `struct UndeliverableSignals {.. }`, moves
/// the three bools rather than removing them and costs `module.rs`'
/// `sweep_is_noteworthy` its flat exhaustive destructure — the compile gate that
/// is the one thing stopping a report from growing a signal that never reaches an
/// operator.
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent per-pass facts, any subset of which can hold at once; see above"
)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// The pass returned early because no registry is wired.
    pub inert: bool,
    /// Tenants holding a pending ref that this pass **found**, which is not what
    /// it swept: the pass stops early on an unconfigured registry, and this counter
    /// is taken from the discovery read before that can happen.
    pub tenants_seen: u64,
    /// Pending refs read this pass, summed over those tenants.
    pub pending_seen: u64,
    /// Distinct `(tenant, version)` pairs handed to the projector.
    pub versions_projected: u64,
    /// Versions this pass ended up judging **complete** — the decision the
    /// D-163 completeness bound gates, carried separately from
    /// [`SweepReport::frontiers_advanced`] because a complete version whose
    /// predecessor is outstanding advances nothing and is still complete.
    pub versions_complete: u64,
    /// Subjects the projector actually wrote a delta for.
    pub subjects_projected: u64,
    /// Subjects whose own transaction refused. Their refs stay pending.
    pub subjects_failed: u64,
    /// Frontier advances this pass produced.
    pub frontiers_advanced: u64,
    /// `PlanPublishDegraded` rows enqueued.
    pub degraded_emitted: u64,
    /// Refs whose age tripped `pricing.catalogversion.commit_overdue`.
    pub commit_overdue: u64,
    /// Tenants whose blocked frontier tripped
    /// `pricing.readmodel.pin_eligibility_overdue`.
    pub pin_eligibility_overdue: u64,
    /// The cross-tenant frontier read that opens the pin-eligibility pass
    /// failed, so that pass saw only the tenants holding a pending ref.
    ///
    /// Reported rather than only logged, and that is the whole point of the
    /// field: this crate has no tracing capture, so a swallowed read had
    /// **nothing** a case could assert and the pass went on answering
    /// `pin_eligibility_overdue: 0`, which reads as healthy. Every other counter
    /// here is unchanged when the read fails — see
    /// [`ReadModelWarmJob::observe_pin_eligibility`] — so no existing member of
    /// this report is a proxy for it.
    pub frontier_scan_failed: bool,
    /// A `PlanPublishDegraded` this pass observed reached no consumer: the enqueue
    /// failed, or the ref named a plan subject the event could not be addressed to.
    ///
    /// **Two causes, one flag**, and deliberately: the field's question is *did a
    /// degradation this pass saw go unreported*, which both answer yes to, and
    /// splitting it would cost `module.rs`' `sweep_is_noteworthy` its flat
    /// exhaustive destructure for a distinction no operator acts on differently.
    /// The second cause — a `subject_ref` that is not a uuid on a
    /// `SubjectKind::Plan` ref — is separately visible as `subjects_failed`,
    /// because the same pass's `project_version` refuses the same row; see
    /// [`ReadModelWarmJob::mark_degraded`].
    ///
    /// [`SweepReport::frontier_scan_failed`]'s sibling, and it exists for the
    /// identical reason. `outbox_repo::enqueue` returns one `RepoError::Db` for
    /// the dedup index refusing a repeat *and* for a store that is down, so the
    /// `Err` arm of [`ReadModelWarmJob::mark_degraded`] cannot tell the expected
    /// case from the fault — and without this field both spell themselves the
    /// same way in the report: [`SweepReport::degraded_emitted`] does not move
    /// on either, and no other member moves at all.
    ///
    /// What goes silent without it is bounded but is the window the signal
    /// exists for: `PlanPublishDegraded` is `fr-publish-fanout-atomicity`'s
    /// signal and fires at `readmodel_degraded_after` (5s), while the nearest
    /// other alarm on the same fault — `pin_eligibility_overdue` — needs
    /// `catalog_version_overdue_after` (300s).
    pub degraded_emit_failed: bool,
    /// The committed-version probe behind the pin-eligibility alarm's second arm
    /// failed to read, so a blocked frontier could not be seen this pass.
    ///
    /// [`ReadModelWarmJob::frontier_is_blocked`] answers `false` on a storage
    /// failure and the reason it gives — *"the pass has already alarmed on the
    /// ref itself"* — holds only where a ref alarm exists. It is asked exactly
    /// when the `waiting` arm above it is **false**, i.e. where the tenant's
    /// pending refs are all younger than the SLO and a committed version
    /// standing above a stale frontier is the only thing that would raise the
    /// Critical. On that path there is no other alarm to fall back on, and every
    /// other counter of this pass is identical between a probe that read and one
    /// that failed.
    pub frontier_block_probe_failed: bool,
}

/// The read-model warm re-drive: one pass, cross-tenant.
pub struct ReadModelWarmJob {
    db: DBProvider<DbError>,
    projector: ReadModelProjector,
    registry: Arc<dyn CatalogVersionRegistryV1>,
    jobs: JobsConfig,
    /// The alarm plane (D-238).
    ///
    /// The two names below are this gear's **only Critical** alarms, and until
    /// D-238 they were the only two not on `PricingAlarm::ALL` — raised as a bare
    /// `tracing::error!` on an argument this module went on making after it
    /// stopped being true. They now go to both: the log line is what the e2e boot
    /// without a registry reads, and the counter is what an alerting rule can see.
    metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
}

impl ReadModelWarmJob {
    /// Build the job over one database provider, the resolved registry (the
    /// fail-closed default when none is wired) and the validated cadences.
    #[must_use]
    pub fn new(
        db: DBProvider<DbError>,
        registry: Arc<dyn CatalogVersionRegistryV1>,
        jobs: JobsConfig,
    ) -> Self {
        let projector = ReadModelProjector::new(db.clone());
        Self {
            db,
            projector,
            registry,
            jobs,
            // The safe default, for `PublishService::new`'s reason: a job built
            // without one reports nothing rather than failing to build.
            metrics: Arc::new(crate::domain::ports::metrics::NoopPricingMetrics),
        }
    }

    /// Attach the metrics port (D-238).
    ///
    /// A **second call** rather than a fourth parameter on [`Self::new`], the
    /// shape `PublishService::with_metrics` and `BundleService::with_metrics`
    /// already use here: every existing caller has nothing to say to it, and the
    /// no-op [`Self::new`] installs is exactly what a test harness means.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// Run one pass, **tenant by tenant**.
    ///
    /// Read under [`AccessScope::allow_all`] with the system actor
    /// [`SecurityContext::anonymous`], narrowing to `AccessScope::for_tenant`
    /// before every per-tenant write — the sanctioned pattern the sibling
    /// ledger's jobs document.
    ///
    /// # Why the shape is per tenant and not one cross-tenant page
    ///
    /// The pending scan has to be bounded, and D-163 clause (2) says the bound
    /// is **per tenant**. On a cross-tenant page ordered by request instant one
    /// tenant's stuck backlog fills the whole budget, and what it defers is every
    /// other tenant's **completions** — so their frontiers do not advance and
    /// their publishes are unpinnable because someone else's are. That is not a
    /// latency detail; it is one tenant's outage becoming another's.
    ///
    /// A per-tenant fault is **isolated**: it is logged and the pass moves to
    /// the next tenant. The refs stay pending, so the next tick re-drives them.
    /// The one thing that stops the whole pass is an **unconfigured** registry,
    /// which is a deployment state rather than a fault.
    ///
    /// `pin_eligibility_overdue` is evaluated once at the end over the union of
    /// every tenant's still-pending refs, because its first half is a
    /// **cross-tenant** frontier read: a per-tenant read cannot find a tenant
    /// whose frontier is stale precisely because nothing of it has moved.
    ///
    /// # Why the pass carries no deadline of its own
    ///
    /// [`registry_ask_timeout`] bounds one cross-gear call and the pass makes them
    /// in sequence, so the tick can lapse under a pass still running. Stopping the
    /// tenant loop at the tick is not the remedy:
    /// [`catalog_version_ref_repo::pending_tenants`] restarts from the lowest tenant
    /// id every pass with no cursor carried across, so under steady load the *same*
    /// tail is abandoned every time — and both Criticals a pass can raise are raised
    /// inside [`Self::sweep_tenant`], so they fall silent for exactly those tenants
    /// while every counter reads as a healthy short pass. A bound that is safe needs
    /// the tail to rotate, which is a change to what `pending_tenants` promises.
    ///
    /// # Errors
    /// [`DomainError::Internal`] only when the pass cannot start — the tenant
    /// discovery read itself failing. Everything after that is isolated within
    /// the pass.
    pub async fn run(&self, now: OffsetDateTime) -> Result<SweepReport, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: warm sweep: {e}")))?;
        let tenants = catalog_version_ref_repo::pending_tenants(
            &conn,
            &AccessScope::allow_all(),
            self.jobs.pending_tenants_per_pass,
        )
        .await
        .map_err(|e| repo_failure(&e))?;

        let mut report = SweepReport {
            tenants_seen: as_count(tenants.len()),
            ..SweepReport::default()
        };
        if tenants.is_empty() {
            return Ok(report);
        }
        let mut still_pending: Vec<PendingVersionRow> = Vec::new();
        // **The pass has no clock of its own, and a per-pass deadline is not the way
        // to give it one.** Stopping the tenant loop at the tick abandons the tail —
        // and [`catalog_version_ref_repo::pending_tenants`] restarts from the lowest
        // tenant id every pass with no cursor carried across, so under steady load it
        // is the *same* tail every time, permanently. Worse than the lapsed lease it
        // would bound: both Criticals this pass can raise are raised inside
        // [`Self::sweep_tenant`], so they fall silent for exactly the tenants that
        // never get swept, while every counter here reads as a healthy short pass.
        //
        // A bound that is safe needs the tail to rotate — a cursor that survives the
        // pass, or a discovery ordered by oldest request rather than by tenant id —
        // and that is a change to what `pending_tenants` promises rather than a knob
        // on this loop.
        for tenant_id in tenants {
            if !self
                .sweep_tenant(&conn, tenant_id, now, &mut report, &mut still_pending)
                .await
            {
                return Ok(report);
            }
        }
        self.observe_pin_eligibility(&conn, &still_pending, now, &mut report)
            .await;
        Ok(report)
    }

    /// One tenant's whole pass: read its pending refs, resolve them, project
    /// what the registry answered, and observe the two signals over what is
    /// still pending afterwards.
    ///
    /// Returns `false` only for an **unconfigured** registry, which stops the
    /// whole sweep; every other failure is isolated to this tenant and answers
    /// `true`.
    async fn sweep_tenant(
        &self,
        conn: &impl DBRunner,
        tenant_id: Uuid,
        now: OffsetDateTime,
        report: &mut SweepReport,
        still_pending: &mut Vec<PendingVersionRow>,
    ) -> bool {
        let bound = self.jobs.pending_refs_per_tenant;
        let Some(pending) = self.read_pending(conn, tenant_id, bound).await else {
            return true;
        };
        if pending.is_empty() {
            return true;
        }
        report.pending_seen += as_count(pending.len());

        // A full page means refs this pass did not see, so no version of this
        // tenant may be judged complete (D-163 clause 2). The count the read
        // already returned says it; no second query is needed to ask.
        let saturated = as_count(pending.len()) >= bound;
        let Some(resolution) = self.resolve(conn, &pending, now, report).await else {
            return false;
        };
        let coverage = if saturated {
            PassCoverage::ScanSaturated
        } else if resolution.any_unresolved {
            PassCoverage::RefUnresolved
        } else {
            PassCoverage::Whole
        };
        // The answers this pass got, by handle. The degraded emission names the
        // publish by its plan and **version** (D-166), and this is where the
        // version is: the ref row cannot carry it while the row is still
        // pending, and the condition being observable at all means some pass
        // asked the registry - which is this one, for every pending ref.
        let observed: BTreeMap<String, CatalogVersion> = resolution
            .by_version
            .iter()
            .flat_map(|(version, rows)| {
                rows.iter()
                    .map(move |row| (row.pending_ref.clone(), *version))
            })
            .collect();
        self.project_all(tenant_id, resolution.by_version, coverage, now, report)
            .await;
        self.observe_tenant(conn, tenant_id, &observed, now, report, still_pending)
            .await;
        true
    }

    /// One tenant's oldest pending refs, or `None` when the read itself failed —
    /// which is isolated to this tenant rather than ending the pass.
    async fn read_pending(
        &self,
        conn: &impl DBRunner,
        tenant_id: Uuid,
        bound: u64,
    ) -> Option<Vec<PendingVersionRow>> {
        match catalog_version_ref_repo::list_pending_for_tenant(
            conn,
            &AccessScope::allow_all(),
            tenant_id,
            bound,
        )
        .await
        {
            Ok(rows) => Some(rows),
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    tenant_id = %tenant_id,
                    "bss-pricing: could not read a tenant's pending refs; the pass continues with \
                     the next tenant and this one is re-driven next tick"
                );
                None
            }
        }
    }

    /// Raise the two per-ref signals over what is **still** pending for this
    /// tenant after its projections, and collect those rows for the
    /// pin-eligibility pass.
    ///
    /// The re-read is load-bearing: the predicate §3.6 states is "**still**
    /// pending past the max batching delay", and this pass has just resolved some
    /// of them. Evaluating it on the registry's answer instead — as an earlier
    /// version did — left two whole classes silent: a registry that **errors**,
    /// and a ref the registry **has** committed whose projection keeps failing,
    /// which is exactly §4.4's "a stuck version now holds the frontier".
    /// Evaluating it on the pre-pass snapshot would alarm for refs this very pass
    /// fixed.
    async fn observe_tenant(
        &self,
        conn: &impl DBRunner,
        tenant_id: Uuid,
        observed: &BTreeMap<String, CatalogVersion>,
        now: OffsetDateTime,
        report: &mut SweepReport,
        still_pending: &mut Vec<PendingVersionRow>,
    ) {
        let Some(rows) = self
            .read_pending(conn, tenant_id, self.jobs.pending_refs_per_tenant)
            .await
        else {
            return;
        };
        for row in &rows {
            self.observe_commit_overdue(row, now, report);
            self.observe_degraded(row, observed.get(&row.pending_ref), now, report)
                .await;
        }
        still_pending.extend(rows);
    }

    /// Raise `pricing.readmodel.pin_eligibility_overdue` on the **frontier's
    /// own age**.
    ///
    /// D-136 and [`PinFrontier::advanced_at`](bss_pricing_sdk::PinFrontier)'s
    /// doc both name `advanced_at` as the referent: it is what the <= 5s pin-lag
    /// rule is measured against and what this alarm fires on. An earlier version
    /// measured a pending ref's age instead, which is `commit_overdue`'s
    /// referent and a different fact — a stuck frontier with nothing pending
    /// went unreported entirely.
    ///
    /// Two conditions, both necessary. **Stale**: the frontier has not moved
    /// within the SLO — a tenant with no frontier row at all counts as stale by
    /// construction, having never advanced. **Blocked**: something the tenant
    /// published is short of pin-eligibility, which is either a committed
    /// version standing above the frontier *or* a ref that has itself been
    /// pending past the SLO. The second arm is not redundant: a version
    /// **every** subject of which fails to project is never committed in
    /// storage at all, because the finalize and the projection share a
    /// transaction — so on the exact path §4.4 calls "a stuck version holds the
    /// frontier", the committed-version arm sees nothing.
    ///
    /// **The age condition on that second arm is what keeps this alarm off
    /// healthy publishes**, and it is the whole of the difference between a
    /// Critical that means something and one that fires twelve times per normal
    /// publish. Staleness alone does not discriminate: a tenant that has simply
    /// not published for five minutes has a stale frontier, and a tenant with
    /// no frontier row is stale by construction — so conjoining staleness with
    /// "has a pending ref" says nothing more than "has a pending ref", which is
    /// true of every publish for the whole of D-47's budget. The ref must be
    /// **overdue**, which is the same threshold `commit_overdue` uses and the
    /// same one `01-foundation.md` §4.4 names ("has not become pin-eligible
    /// within the max batching-delay SLO").
    ///
    /// The tenant set is every tenant with a frontier row, plus the tenants of
    /// this pass's still-pending refs. The first half is what finds a tenant
    /// whose frontier is stale precisely because nothing of it has moved — and
    /// when the read producing that half fails, the pass says so on
    /// [`SweepReport::frontier_scan_failed`] and at `error` rather than
    /// continuing over the other half in silence.
    async fn observe_pin_eligibility(
        &self,
        conn: &impl DBRunner,
        pending: &[PendingVersionRow],
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) {
        let threshold = match time::Duration::try_from(self.jobs.catalog_version_overdue_after()) {
            Ok(threshold) => threshold,
            Err(e) => {
                unconvertible_threshold(
                    e,
                    "catalog_version_overdue_after",
                    ALARM_PIN_ELIGIBILITY_OVERDUE,
                );
                return;
            }
        };
        let mut tenants = self.frontier_ages(conn, report).await;
        // **Whether that scan answered is what decides the probe's shape below**,
        // so it is read before the pending tenants are folded in: after that fold
        // a `None` means either "no frontier row" or "the scan did not run", and
        // those two want different reads.
        let scanned = !report.frontier_scan_failed;
        // The oldest pending request per tenant, folded once because the probe
        // below asks it per tenant. Rescanning the whole `pending` slice there makes
        // the pass O(tenants x pending refs) — at `FRONTIER_SCAN_LIMIT` tenants
        // against the per-pass ceiling of
        // `pending_tenants_per_pass x pending_refs_per_tenant` that is a five-figure
        // scan on a five-second tick, for an answer one map holds.
        let mut oldest_pending: BTreeMap<Uuid, OffsetDateTime> = BTreeMap::new();
        for row in pending {
            oldest_pending
                .entry(row.tenant_id)
                .and_modify(|at| {
                    if row.requested_at < *at {
                        *at = row.requested_at;
                    }
                })
                .or_insert(row.requested_at);
            tenants.entry(row.tenant_id).or_insert(None);
        }

        for (tenant_id, frontier) in tenants {
            let stale = frontier
                .as_ref()
                .is_none_or(|f| (now - f.advanced_at) >= threshold);
            if !stale {
                continue;
            }
            // The ref must itself be **past the SLO**, not merely exist.
            // "This tenant has something pending" is true of every healthy
            // publish for the whole of D-47's batching budget, and conjoined
            // with a staleness test that a tenant simply not publishing for
            // five minutes satisfies, it degenerates into "this tenant has a
            // pending ref" - roughly twelve Critical alarms per normal publish
            // at a five-second tick.
            let waiting = oldest_pending
                .get(&tenant_id)
                .is_some_and(|at| (now - *at) >= threshold);
            // **The frontier row this pass already read, not a second read of
            // it**. `frontier_is_blocked` used
            // to open with its own `pin_frontier_repo::read_at`, which returns the
            // row `frontier_ages` had just read for every tenant in this map — and
            // it is reached for **every stale, non-waiting tenant**, which at the
            // ratified `catalog_version_overdue_secs = 300` is every tenant that
            // has not published in five minutes, i.e. essentially the whole
            // `FRONTIER_SCAN_LIMIT` page, on every 5s tick that any one tenant
            // holds a pending ref. Half of that traffic was re-reading a row the
            // pass held.
            //
            // The fallback stays for the one case where the pass does *not* hold
            // it: a failed cross-tenant scan leaves the map carrying only the
            // pending-ref tenants, all marked `None`, and taking that `None` as
            // "this tenant has no frontier" would ask for the lowest committed
            // version the tenant ever had and raise a Critical on a healthy
            // estate. So when the scan failed the per-tenant read is still made.
            let standing = if scanned {
                Standing::Read(frontier.as_ref().map(|f| f.catalog_version))
            } else {
                Standing::Unknown
            };
            if !waiting
                && !self
                    .frontier_is_blocked(conn, tenant_id, standing, report)
                    .await
            {
                continue;
            }
            tracing::error!(
                alarm = ALARM_PIN_ELIGIBILITY_OVERDUE,
                tenant_id = %tenant_id,
                advanced_at = ?frontier.as_ref().map(|f| f.advanced_at),
                "bss-pricing: a committed catalog version has stood short of pin-eligibility \
                 past the max batching delay; consumers keep pinning the previous edge"
            );
            self.metrics
                .alarm(crate::domain::ports::metrics::PricingAlarm::ReadModelPinEligibilityOverdue);
            report.pin_eligibility_overdue += 1;
        }
    }

    /// Every tenant that has a frontier row, with the instant it last advanced.
    ///
    /// **The half of [`Self::observe_pin_eligibility`]'s population that nothing
    /// else in the pass produces**, and the reason its failure may not be
    /// swallowed. This was an `if let Ok` with no `else`: on a storage
    /// failure the map came back empty, the tenant set degenerated to *only the
    /// tenants holding a pending ref this pass* — precisely the population this
    /// read exists to go beyond — and the pass reported
    /// `pin_eligibility_overdue: 0`, which reads as healthy.
    ///
    /// [`Self::frontier_is_blocked`] makes the same `Ok`-only choice and
    /// documents it: *"an alarm that cannot read must not become a second fault,
    /// and the pass has already alarmed on the ref itself"*. That argument does
    /// not transfer to this call, because the tenants it finds are by
    /// construction the ones nothing else in the pass has alarmed about.
    ///
    /// Reported as well as logged. [`Self::read_pending`]'s `error!` is the shape
    /// the log takes; [`SweepReport::frontier_scan_failed`] is what a case can
    /// assert, and without it the swallow was untestable — every other counter
    /// this pass produces is identical between a pass that read the frontiers and
    /// one whose read failed.
    async fn frontier_ages(
        &self,
        conn: &impl DBRunner,
        report: &mut SweepReport,
    ) -> BTreeMap<Uuid, Option<PinFrontier>> {
        match pin_frontier_repo::list_all(conn, &AccessScope::allow_all(), FRONTIER_SCAN_LIMIT)
            .await
        {
            Ok(frontiers) => frontiers
                .into_iter()
                .map(|(tenant_id, frontier)| (tenant_id, Some(frontier)))
                .collect(),
            Err(e) => {
                report.frontier_scan_failed = true;
                // **Not stamped `alarm = …`.** That string is what an operator's
                // runbook greps for and what `metrics.alarm` labels; putting it
                // here would report the Critical as *raised*, where what happened
                // is that it could not be evaluated. The guard is named instead.
                tracing::error!(
                    error = ?e,
                    degraded_guard = ALARM_PIN_ELIGIBILITY_OVERDUE,
                    "bss-pricing: the cross-tenant pin frontier read failed, so this pass judged \
                     pin-eligibility only over the tenants holding a pending ref; a tenant whose \
                     frontier is stale precisely because nothing of it has moved cannot be seen \
                     this tick, and the Critical it would raise is not raised"
                );
                BTreeMap::new()
            }
        }
    }

    /// Ask the registry what version each pending handle landed in.
    ///
    /// `None` is the **inert** answer — no registry is wired — and it is
    /// returned rather than reported as an error for the reason the module doc
    /// gives: an unconfigured registry is a deployment state, not a catalog
    /// defect, and a pass alarming about it every five seconds would drown the
    /// boot that depends on it.
    ///
    /// Grouping into a `BTreeMap` keyed by version is what makes a tenant's
    /// versions arrive in **ascending** order at the projector, so the frontier's
    /// next-version-in-order check never sees a gap this very pass is about to
    /// fill.
    ///
    /// **The commit observation is stamped here** (D-166 clause 1), on the
    /// `Ok(Some(_))` arm, once per handle beside the question that produced it,
    /// in a statement of its own **before** any projection is attempted. That is not tidiness: the finalize and the warm share one
    /// transaction, so an observation written inside it would roll back on
    /// exactly the path — a committed version whose projection keeps failing —
    /// that the degraded signal exists to name. A stamp that failed to write is
    /// logged and the resolution continues: the next pass re-observes, so the
    /// signal is delayed by a tick rather than lost.
    ///
    /// **A registry error is recorded as coverage, not only logged** (D-163
    /// clause 2). It leaves a sibling of some version at an unknown version, so
    /// this pass may not judge any of this tenant's versions complete — see
    /// [`PassCoverage`] for why `Ok(None)` is not the same fact.
    ///
    /// # One question per **handle**, over rows that are per **subject** (D-234)
    ///
    /// `committed_version` takes a `pending_ref` and nothing else, so every
    /// subject of one handle asks one question — and since a publish unit records
    /// every subject it projects against a single handle (two or three on the
    /// overlay plane, D-112/D-133), the rows arriving here repeat handles. The
    /// answer is cached for the length of the pass, which bounds the sweep's
    /// registry traffic by the number of publish **acts** rather than by the
    /// number of subjects they project.
    ///
    /// The cache is per pass and never wider. A handle's answer moves — that is
    /// the whole of what the sweep is watching for — so carrying one across ticks
    /// would be caching the thing being observed.
    async fn resolve(
        &self,
        conn: &impl DBRunner,
        pending: &[PendingVersionRow],
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) -> Option<Resolution> {
        let ctx = SecurityContext::anonymous();
        let mut resolution = Resolution::default();
        let mut answered: BTreeMap<String, HandleAnswer> = BTreeMap::new();
        for row in pending {
            let answer = if let Some(known) = answered.get(&row.pending_ref) {
                *known
            } else {
                let Some(fresh) = self.ask_registry(&ctx, &row.pending_ref).await else {
                    // The pass-ending answer, and the flag is set here rather
                    // than in the ask: `inert` is a fact about this pass, and
                    // the ask reports only about the handle.
                    report.inert = true;
                    return None;
                };
                answered.insert(row.pending_ref.clone(), fresh);
                // Per **handle**, beside the question it follows. The repo
                // statement filters on `(tenant, pending_ref)` and write-once on
                // `commit_observed_at IS NULL`, so it stamps every row of the
                // handle at once; driving it per row repeats the identical
                // update two or three times per overlay publish (D-112/D-133) on
                // a five-second tick, which is the traffic the answer cache
                // above exists to bound.
                if matches!(fresh, HandleAnswer::Committed(_)) {
                    self.observe_commit(conn, row, now).await;
                }
                fresh
            };
            match answer {
                // The outage arm. The warning is logged where the answer was
                // fetched, once per handle; what is recorded here is per **row**,
                // because coverage is a statement about the subject set.
                HandleAnswer::Unresolved => {
                    resolution.any_unresolved = true;
                }
                // Not committed yet. Not an error and not an alarm here - the
                // registry batches, and the wait is budgeted. Its age is
                // observed by the pass, uniformly with every other answer.
                HandleAnswer::NotYet => {}
                HandleAnswer::Committed(version) => {
                    resolution
                        .by_version
                        .entry(version)
                        .or_default()
                        .push(row.clone());
                }
            }
        }
        Some(resolution)
    }

    /// Ask the registry about **one handle**, and classify the answer.
    ///
    /// Split out of [`resolve`](Self::resolve) so the loop reads as "one answer
    /// per handle, one record per row": the outage warning belongs here, where
    /// the question was actually asked, and the coverage record belongs there,
    /// where the subject set is being counted.
    ///
    /// `None` is the pass-ending answer — an unconfigured registry — and it is
    /// propagated with `?` rather than folded into [`HandleAnswer`], because it
    /// is a fact about the deployment rather than about the handle.
    async fn ask_registry(&self, ctx: &SecurityContext, pending_ref: &str) -> Option<HandleAnswer> {
        // **Bounded, because the peer is another gear and this pass has no other
        // clock.** Up to `pending_tenants_per_pass x pending_refs_per_tenant`
        // sequential cross-gear calls run here, and an unbounded one stalls the only
        // path that turns a committed publish into something a consumer can pin —
        // with no signal but the sweep not returning, and with the tick's
        // coordination lease lapsing underneath it so peers start concurrent passes.
        //
        // An elapsed call is `Unresolved`, not an error: "the registry did not
        // answer in time" costs the pass its right to judge completeness in exactly
        // the way "the registry answered with a failure" does, and the ref stays
        // pending either way.
        let budget = registry_ask_timeout();
        let asked =
            tokio::time::timeout(budget, self.registry.committed_version(ctx, pending_ref)).await;
        let Ok(answered) = asked else {
            tracing::warn!(
                pending_ref = %pending_ref,
                timeout_secs = budget.as_secs(),
                "bss-pricing: registry did not answer a pending version ref in time; no \
                 completion is decided for this tenant on this pass"
            );
            return Some(HandleAnswer::Unresolved);
        };
        Self::classify(
            pending_ref,
            answered.map_err(CatalogVersionRegistryError::from),
        )
    }

    /// What one registry answer means to the pass.
    ///
    /// Split from the ask for `clippy::cognitive_complexity`, and the split is the
    /// honest seam: the ask is about the clock, this is about the answer.
    fn classify(
        pending_ref: &str,
        answered: Result<Option<CatalogVersion>, CatalogVersionRegistryError>,
    ) -> Option<HandleAnswer> {
        match answered {
            Err(CatalogVersionRegistryError::Unconfigured) => {
                tracing::debug!(
                    "bss-pricing: read-model warm sweep skipped (no CatalogVersion registry \
                     configured)"
                );
                None
            }
            Err(e) => {
                // A configured registry that cannot answer is a transient
                // outage, not a catalog defect: the ref stays pending and its
                // age is what eventually alarms. What the outage DOES cost is
                // this pass's right to judge completeness, because a ref of
                // unknown version may belong to a version this pass is about to
                // call complete.
                tracing::warn!(
                    error = %e,
                    pending_ref = %pending_ref,
                    "bss-pricing: registry could not resolve a pending version ref; no \
                     completion is decided for this tenant on this pass"
                );
                Some(HandleAnswer::Unresolved)
            }
            Ok(None) => Some(HandleAnswer::NotYet),
            Ok(Some(version)) => Some(HandleAnswer::Committed(version)),
        }
    }

    /// Stamp `commit_observed_at` on a ref whose commit this pass has just seen.
    ///
    /// Write-once in the repository, so the instant stays the **first** sighting:
    /// a stamp that advanced every pass would hold the degraded clock at zero
    /// forever and the signal would never raise.
    ///
    /// Under the tenant's own scope, not the system one — the same narrowing
    /// every other per-tenant write in this pass does.
    async fn observe_commit(
        &self,
        conn: &impl DBRunner,
        row: &PendingVersionRow,
        now: OffsetDateTime,
    ) {
        if row.commit_observed_at.is_some() {
            return;
        }
        let scope = AccessScope::for_tenant(row.tenant_id);
        if let Err(e) = catalog_version_ref_repo::observe_commit(
            conn,
            &scope,
            row.tenant_id,
            &row.pending_ref,
            now,
        )
        .await
        {
            tracing::warn!(
                error = ?e,
                pending_ref = %row.pending_ref,
                "bss-pricing: could not record the commit observation; the next pass re-observes \
                 and the degraded signal is a tick late rather than lost"
            );
        }
    }

    /// Drive the projector over every version this tenant resolved into,
    /// isolating faults.
    ///
    /// A per-version failure is logged and the pass continues: one unprojectable
    /// plan must not stop the tenant's other publishes from becoming pinnable.
    /// The ref stays pending, so the next tick re-drives it and its age
    /// eventually trips `commit_overdue`.
    ///
    /// `coverage` rides through unchanged; the projector is where it gates the
    /// completeness decision, because that is where both decisions are made.
    async fn project_all(
        &self,
        tenant_id: Uuid,
        by_version: BTreeMap<CatalogVersion, Vec<PendingVersionRow>>,
        coverage: PassCoverage,
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) {
        let scope = AccessScope::for_tenant(tenant_id);
        for (version, rows) in by_version {
            match self
                .projector
                .project_version(&scope, tenant_id, version, &rows, coverage, now)
                .await
            {
                Ok(outcome) => {
                    report.versions_projected += 1;
                    report.subjects_projected += as_count(outcome.projected);
                    report.subjects_failed += as_count(outcome.failed);
                    if outcome.complete {
                        report.versions_complete += 1;
                    }
                    if outcome.frontier_advanced_to.is_some() {
                        report.frontiers_advanced += 1;
                    }
                }
                Err(e) => tracing::error!(
                    error = %e,
                    tenant_id = %tenant_id,
                    catalog_version = version.get(),
                    "bss-pricing: projecting a committed catalog version failed; its refs stay \
                     pending and the next tick re-drives them"
                ),
            }
        }
    }

    /// **The registry has not answered.** A ref pending, its commit **not yet
    /// observed**, past the max batching-delay SLO — §3.6's predicate as D-166
    /// clause (4) narrows it.
    ///
    /// The narrowing is the whole of the D-166 repair on this side: without it
    /// this alarm also fires on a ref the registry *has* committed whose warm is
    /// failing, which is a different fault with a different remedy, and an
    /// operator holding both under one string cannot tell which they have.
    ///
    /// Evaluated for every still-pending ref whatever the registry said **this**
    /// pass, because the qualification is on the row's recorded observation and
    /// not on this pass's answer: a registry that errors leaves a ref ageing
    /// with no observation, which is exactly the outage the alarm most needs to
    /// name.
    fn observe_commit_overdue(
        &self,
        row: &PendingVersionRow,
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) {
        if row.commit_observed_at.is_some() {
            return;
        }
        let waited = now - row.requested_at;
        let threshold = match time::Duration::try_from(self.jobs.catalog_version_overdue_after()) {
            Ok(threshold) => threshold,
            Err(e) => {
                unconvertible_threshold(e, "catalog_version_overdue_after", ALARM_COMMIT_OVERDUE);
                return;
            }
        };
        if waited < threshold {
            return;
        }

        tracing::error!(
            alarm = ALARM_COMMIT_OVERDUE,
            tenant_id = %row.tenant_id,
            pending_ref = %row.pending_ref,
            subject_kind = %row.subject_kind,
            subject_ref = %row.subject_ref,
            waited_secs = waited.whole_seconds(),
            "bss-pricing: a catalog version request has stood pending past the max batching \
             delay with no answer from the registry; remediation is a registry re-request, never \
             a silent re-emit"
        );
        self.metrics
            .alarm(crate::domain::ports::metrics::PricingAlarm::CatalogVersionCommitOverdue);
        report.commit_overdue += 1;
    }

    /// **The registry answered and the warm has not landed.** A ref still
    /// unresolved whose commit **was** observed, older than §1.2's 5s
    /// propagation SLO — `fr-publish-fanout-atomicity`'s own condition, D-166
    /// clause (2).
    ///
    /// `observed` is this pass's answer for the handle, and it is what names the
    /// version in the event's dedup key. It is absent only when the registry
    /// errored for this specific ref on this pass, having answered on an earlier
    /// one; the emission then waits for the next pass rather than being keyed on
    /// something else, because a second keying of one degradation is a second
    /// event for it.
    async fn observe_degraded(
        &self,
        row: &PendingVersionRow,
        observed: Option<&CatalogVersion>,
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) {
        let Some(seen_at) = row.commit_observed_at else {
            return;
        };
        let threshold = match time::Duration::try_from(self.jobs.readmodel_degraded_after()) {
            Ok(threshold) => threshold,
            Err(e) => {
                unconvertible_threshold(
                    e,
                    "readmodel_degraded_after",
                    CatalogEvent::PlanPublishDegraded.as_str(),
                );
                return;
            }
        };
        if (now - seen_at) < threshold {
            return;
        }
        let Some(version) = observed.copied() else {
            tracing::debug!(
                pending_ref = %row.pending_ref,
                "bss-pricing: a degraded publish's version is unknown this pass (the registry did \
                 not answer); the emission waits for the next one"
            );
            return;
        };

        if self.mark_degraded(row, version, seen_at, now, report).await {
            report.degraded_emitted += 1;
        }
    }

    /// Does a **committed** version stand above this tenant's frontier?
    ///
    /// One of the two arms of "blocked"; see
    /// [`ReadModelWarmJob::observe_pin_eligibility`] for why it is not enough on
    /// its own.
    ///
    /// The frontier itself is **not** re-read on the ordinary path: [`Standing`]
    /// carries what the pass's own cross-tenant scan already found, and only a
    /// failed scan sends this back to `pin_frontier_repo::read_at`. See
    /// [`Standing`] for why that is a two-armed type rather than an `Option`.
    ///
    /// Read under the tenant's own scope, not the system one: the
    /// `next_committed_version_after` read below is a per-tenant read on behalf
    /// of a per-tenant alarm, and narrowing here is the same discipline the
    /// writes below follow. The frontier row the scan supplies came from the
    /// system-scoped read the pass has always made for it.
    ///
    /// **A storage failure still answers `false`, and now says so.** The
    /// direction is unchanged — an alarm that cannot read must not become a
    /// second fault — but the reason this arm used to give for staying silent,
    /// *"the pass has already alarmed on the ref itself"*, does not hold where
    /// it is asked. It is reached only when the `waiting` arm above it is
    /// **false**: the tenant's pending refs are all younger than the SLO, so
    /// there is no ref alarm to fall back on and a committed version standing
    /// above a stale frontier is the only thing that would raise the Critical.
    /// [`SweepReport::frontier_block_probe_failed`] is what a case and an
    /// operator can see, `frontier_ages`' rule one screen up applied to the
    /// swallow it left standing.
    async fn frontier_is_blocked(
        &self,
        conn: &impl DBRunner,
        tenant_id: Uuid,
        standing: Standing,
        report: &mut SweepReport,
    ) -> bool {
        let scope = AccessScope::for_tenant(tenant_id);
        let standing = match standing {
            // The row the cross-tenant scan already read this pass. Not a second
            // read of it; see the call site.
            Standing::Read(version) => version,
            Standing::Unknown => match pin_frontier_repo::read_at(conn, &scope, tenant_id).await {
                Ok(standing) => standing.map(|frontier| frontier.catalog_version),
                Err(e) => {
                    report.frontier_block_probe_failed = true;
                    // `degraded_guard`, never `alarm` — `frontier_ages`' rule: the
                    // Critical was not raised, it could not be evaluated, and an
                    // operator's runbook greps the other string.
                    tracing::warn!(
                        error = ?e,
                        tenant_id = %tenant_id,
                        degraded_guard = ALARM_PIN_ELIGIBILITY_OVERDUE,
                        "bss-pricing: this tenant's pin frontier could not be read, so the \
                         committed-version arm of pin-eligibility was not evaluated; the \
                         tenant's pending refs are all inside the SLO, so nothing else this \
                         pass alarms on it"
                    );
                    return false;
                }
            },
        };
        match catalog_version_ref_repo::next_committed_version_after(
            conn, &scope, tenant_id, standing,
        )
        .await
        {
            Ok(standing_above) => standing_above.is_some(),
            Err(e) => {
                report.frontier_block_probe_failed = true;
                tracing::warn!(
                    error = ?e,
                    tenant_id = %tenant_id,
                    degraded_guard = ALARM_PIN_ELIGIBILITY_OVERDUE,
                    "bss-pricing: the committed-version read above this tenant's frontier failed, \
                     so the arm of pin-eligibility that does not need a pending ref was not \
                     evaluated"
                );
                false
            }
        }
    }

    /// Enqueue one `PlanPublishDegraded` for a plan publish still waiting.
    ///
    /// **No check-then-enqueue.** `uq_pricing_outbox_dedup_key` is what makes a
    /// repeat of one degradation one event, and a pre-check would be a read the
    /// insert races with anyway — the posture `outbox_repo::enqueue`'s own doc
    /// takes for both refusals that index makes. A repeat therefore arrives as
    /// a storage refusal.
    ///
    /// **The `Err` arm is not the repeat, and it cannot be.** `enqueue`'s own
    /// contract says `RepoError::Db` *"includes the two refusals this table's
    /// indexes make"*, so the dedup refusal and a store that is down are one
    /// variant carrying different strings. This arm used to name the repeat, log
    /// at `debug` and answer `false` — the shape `frontier_ages` was found and
    /// fixed for one screen up. It now sets
    /// [`SweepReport::degraded_emit_failed`] and logs at `warn`, which is what
    /// makes the two outcomes distinguishable to a case and to an operator
    /// without inventing a `RepoError` variant this layer cannot mint. The
    /// wording stays honest about which of the two it is, because the code still
    /// cannot tell.
    ///
    /// The **correlation id is minted per emission** because a background pass
    /// is its own causal origin: the ref row carries none, the publish that
    /// requested the handle is long committed, and borrowing that publish's
    /// correlation id would attribute a sweep's observation to a request that
    /// did not make it.
    ///
    /// Only `plan` subjects are marked, and **not because the others cannot
    /// occur** (D-234). The projector answers **every** `SubjectKind`, an overlay
    /// publish writes one ref row per subject, and `list_pending_for_tenant` carries
    /// no subject-kind predicate — so those rows do reach this method, and are
    /// refused here rather than never arriving. (No kind is refused by name any
    /// more; `subject_of`'s own doc records the last of those going. D-237's reason
    /// for the plan-only filter never depended on any kind being unbuildable.)
    ///
    /// **The reason is what the event is** (D-237). `PlanPublishDegraded` names
    /// a `plan_id` and is addressed to a consumer of that plan; what it adds
    /// over `commit_overdue` is which of two waits is failing, not which
    /// subject. The subject-generic hazard — *this tenant's pin frontier is not
    /// advancing* — is already carried by `pin_eligibility_overdue`, whose
    /// predicate reads tenant and age and never subject kind, and a version is
    /// incomplete exactly while it holds a pending ref, so an unwarmed overlay
    /// holds the frontier exactly as a plan does. Nothing a consumer could act
    /// on is lost with it: the frontier does not advance past an incomplete
    /// version, so no pin ever resolves one whose overlay is unwarmed — the
    /// overlay has simply not taken effect, which is the safe reading.
    ///
    /// `sqlite_read_model::an_overlay_subjects_stuck_warm_raises_the_frontier_alarm_and_no_degraded_event`
    /// is that argument as a case, and it asserts **both** halves: the silence
    /// alone would also pass against a pass that alarmed on nothing at all.
    async fn mark_degraded(
        &self,
        row: &PendingVersionRow,
        catalog_version: CatalogVersion,
        commit_observed_at: OffsetDateTime,
        now: OffsetDateTime,
        report: &mut SweepReport,
    ) -> bool {
        if row.subject_kind != SubjectKind::Plan {
            return false;
        }
        let Ok(plan_id) = Uuid::parse_str(&row.subject_ref) else {
            // **The two `false` arms are not the same fact and must not be spelled
            // the same way**. The arm above is the ordinary
            // "this subject is not a plan"; this one is a plan ref whose
            // `subject_ref` is not a uuid, which `read_model::subject_uuid`
            // refuses as `DomainError::Internal` and no writer in the gear can
            // produce.
            //
            // What is lost is bounded and worth stating precisely, because the
            // finding that opened this over-claimed it: the **same** pass already
            // refuses the same row loudly — `sweep_tenant` runs `project_all`
            // before `observe_tenant`, and `project_version` answers a per-subject
            // refusal with a `tracing::error!` plus `subjects_failed += 1`. So an
            // operator is not blind and a case can assert it. What is lost is this
            // publish's `PlanPublishDegraded` and, without the two lines below,
            // any way to tell this arm from the one above.
            report.degraded_emit_failed = true;
            tracing::warn!(
                tenant_id = %row.tenant_id,
                pending_ref = %row.pending_ref,
                subject_ref = %row.subject_ref,
                degraded_guard = CatalogEvent::PlanPublishDegraded.as_str(),
                "bss-pricing: PlanPublishDegraded was not addressable - this pending ref names a \
                 plan subject whose subject_ref is not a uuid, so the degradation reached no \
                 consumer; the same pass's projector refuses the same row into subjects_failed"
            );
            return false;
        };
        let payload = PlanPublishDegradedPayload {
            plan_id: PlanId::new(plan_id),
            catalog_version,
            pending_version_ref: row.pending_ref.clone(),
            commit_observed_at,
            // Minted, and it is the one place in the gear that still is. D-178
            // clause (2) says the correlation belongs to the operator call — and
            // this alarm has no operator call: it is raised by a periodic sweep
            // observing a publish that committed in some other process, possibly
            // days earlier, whose correlation this job never saw. "An in-process
            // producer supplies its own" is the clause that covers it. What the
            // value still buys is that the alarm's own emission is nameable.
            //
            // The granularity is stated rather than left to be inferred: it is
            // **per alarm**, not per sweep, so two degraded handles observed by
            // one pass carry two values and nothing says they were seen together.
            // That is the right reading of clause (2) — the clause is about an
            // operator call, and a sweep is not one — but it does mean the sweep
            // is not itself a joinable unit. Nothing needs it to be today.
            correlation_id: Uuid::now_v7(),
        };
        let scope = AccessScope::for_tenant(row.tenant_id);
        let tenant_id = row.tenant_id;
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<u64, DomainError, _>(move |txn| {
                Box::pin(async move {
                    outbox_repo::enqueue(
                        txn,
                        &scope,
                        NewOutboxEvent::plan_publish_degraded(tenant_id, &payload, now),
                    )
                    .await
                    .map_err(|e| repo_failure(&e))
                })
            })
            .await;
        match outcome {
            Ok(_) => true,
            Err(e) => {
                report.degraded_emit_failed = true;
                // `degraded_guard`, never `alarm` — see `frontier_ages`. The
                // ordinary case is the dedup index refusing a repeat of a
                // degradation an earlier pass already reported, and the
                // exceptional one is a store that will not take the write; this
                // arm receives both as one variant, so it names both rather than
                // asserting the benign reading.
                tracing::warn!(
                    error = ?e,
                    tenant_id = %row.tenant_id,
                    pending_ref = %row.pending_ref,
                    degraded_guard = CatalogEvent::PlanPublishDegraded.as_str(),
                    "bss-pricing: PlanPublishDegraded was not enqueued - either an earlier pass \
                     already reported this degradation and the dedup index refused the repeat, or \
                     the store refused the write and this degradation reached no consumer"
                );
                false
            }
        }
    }
}

/// A count for the report, saturating rather than casting.
///
/// `usize -> u64` is infallible on every target this builds for, but `as` is
/// banned here and a `TryFrom` on a counter would put a `Result` in a report
/// that has nothing to say about one.
fn as_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "readmodel_warm_tests.rs"]
mod readmodel_warm_tests;
