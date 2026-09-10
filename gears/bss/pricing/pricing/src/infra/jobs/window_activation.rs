//! `WindowActivationJob` — the UTC activation/expiry sweep, and this gear's
//! first producer of any `PriceWindow*` event.
//!
//! §1.7's `WindowActivationJob`, verbatim: *"Coordination-lease singleton: flips
//! `scheduled → active` at `effectiveFrom` and `active → expired` at
//! `effectiveTo` (UTC, idempotent, ordered per `(tenant, plan)`), emitting
//! `PriceWindowActivated`/`Expired` from the outbox"*. One pass reads the two
//! due sets, orders the work, and flips each window with its event **in one
//! transaction**.
//!
//! # It is not a publish unit, and that is the design rather than an omission
//!
//! `inst-ws-publishunit` (D-99) makes every window *mutation* a publish unit —
//! validation, a pending `CatalogVersion` ref, a plan-subject re-projection —
//! and says in the same breath that **activation and expiry are not**: the read
//! model carries window **intervals**, so a consumer re-derives "active at `t`"
//! from a frozen interval and `now` crossing an `effectiveFrom` changes a truth
//! row and changes nothing projected. So this job requests no version, writes no
//! `pricing_read_model` row and calls no projector, and the assertion that it
//! does not is
//! `tests/sqlite_read_model.rs::an_activation_re_projects_nothing_and_the_frozen_delta_answers_anyway`.
//!
//! # What idempotence is made of here — three mechanisms, none of them a marker
//!
//! 1. **The due read.** [`window_repo::list_due`] selects on `state` as well as
//!    the boundary, so a second pass over the same instant matches zero rows.
//!    There is no "swept" column and no cursor; the state *is* the record of
//!    what has been done.
//! 2. **The `UPDATE`'s own predicate.** `window_repo::transition` carries
//!    `state = <expected>` and §4's condition into its `WHERE`, so a flip that
//!    lost a race matches nothing rather than trusting the read that preceded
//!    it.
//! 3. **The dedup key.** [`outbox_repo::price_window_transition_dedup_key`]
//!    covers `(window_id, transition)`, and the second row is refused by the two
//!    constraints that key reaches — `uq_pricing_outbox_dedup_key` and the
//!    `outbox_id` primary key derived from the same pair, either of which is enough
//!    and neither of which the driver's error class can name. So one flip produces
//!    exactly one event. This is the mechanism the coordination lease would
//!    otherwise have had to be, and a lease cannot be one: it can be lost.
//!
//!    The flip and the enqueue also share **one transaction**, which is a
//!    different guarantee and is worth not conflating with this one: it is what
//!    stops a committed flip from standing without its event. In a lost race there
//!    is nothing to roll back on the window side at all — the `UPDATE` matched zero
//!    rows — so `tests/postgres_window_activation.rs` proves "one flip, one event"
//!    and does not exercise the transaction; that property holds by the shape of
//!    [`WindowActivationJob::flip`] rather than by a test.
//!
//! # The alarm, and the sink this module spent a slice denying
//!
//! `pricing.window.activation_overdue` (Warn, §7) is the design set's string,
//! taken from it rather than invented. It now goes to **both** planes: the
//! `tracing::error!` carries the window an operator needs in order to act, and
//! `metrics.alarm(…)` is what an alerting rule can match.
//!
//! **This gear has an alarm facility**, and the claim that it has none — *"the
//! sibling ledger has `infra/metrics.rs` … and this crate has neither"* — is the one
//! this section is most likely to inherit. `domain::ports::metrics` is where it
//! lives; a doc and a unit test can both carry the older claim past the slice that
//! falsifies it.
//!
//! D-238 was opened to close exactly this, corrected `readmodel_warm`'s two
//! Criticals, and was filed BUILT — so this was a live third instance under a
//! closed decision, which is the state nobody goes looking in. The consequence
//! was not cosmetic: this is §7's only window-plane alarm, its text is "the
//! lease singleton is stalled", and it reaches `pricing_alarm_total` on no label
//! at all. The estate could see every Info this gear raises and not the one that
//! says the gear stopped working.
//!
//! ## Its condition is read off the due set, before the flip
//!
//! §7: *"a `scheduled` window past `effectiveFrom` (or an `active` one past
//! `effectiveTo`) **not yet transitioned** beyond the job SLO; the lease
//! singleton is stalled"*. "Not yet transitioned" is the state of the row **as
//! this pass finds it**, so that is where the condition is evaluated — a window
//! whose start passed an hour ago and is only being flipped now went an hour
//! untransitioned, whether or not the flip then succeeds.
//!
//! The alternative — alarm only on what the pass **failed** to flip — was
//! rejected, and not on taste: it goes silent on exactly the case §7 names. A
//! stalled singleton flips nothing at all, and the pass that eventually takes the
//! lease clears the whole backlog successfully; under that reading the outage
//! produces no signal, ever.
//!
//! What the alarm cannot see either way is a deployment where **no** replica
//! sweeps: the observer is the pass. A stalled *holder* is covered, because the
//! lease TTL frees the slot and the peer that takes it finds the backlog with old
//! boundaries; a stalled *deployment* is not, and no signal inside this crate
//! could be — that is what an external liveness probe is for, and it is reported
//! rather than papered over.
//!
//! ## The age threshold, and why it is a knob rather than a constant
//!
//! Without an age bound this alarm fires for every window whose instant merely
//! arrived between two ticks — once each, on the pass that behaved perfectly.
//! `jobs.window_activation_overdue_secs` is the bound, in the shape
//! `readmodel_warm` gives `catalog_version_overdue_after()`.
//!
//! **The design set names no value for it, and that is a divergence rather than a
//! reading.** §10 says the activation job's SLO *"rides the ratified NFR set
//! (2026-07-28, PRD §14)"*, and that set has no activation entry at all — its
//! rows are publish propagation, read latency, validation, event delivery,
//! currency floor and repricing throughput. So the deployment states the value
//! and the default is stated with its reasoning and nothing more: 300s, the
//! order of the ratified max batching delay (D-47), because a window that has not
//! flipped for five minutes is late by every other clock in this gear. Reported.
//!
//! # What this job does **not** write, and why that is finished rather than filed
//!
//! `pricing_audit_log` gets nothing. Two questions hide in that, and only the first
//! was answered when this was written.
//!
//! **Who.** Nobody: a time-driven flip has no operator to attribute, `inst-au-pii`
//! makes the audit actor a pseudonymous principal id, and inventing one would be a
//! principal that does not exist. [`SYSTEM_ACTOR`] is the nil uuid for that reason;
//! see its doc.
//!
//! **Whether a state change on a row retained ≥ 7 years needs a record at all,
//! whoever caused it.** It does — `dod-audit` is "every mutation MUST record
//! actor/timestamp/before-after", with no carve-out for machine-driven ones — and
//! the record exists, on the same retention horizon, in two places the design set
//! itself nominates: the row's `activated_at`/`expired_at`, which §6 labels *audit
//! timestamps*, and the outbox event, whose `enqueued_at` is when the flip was
//! performed. Nothing about a §4 flip is unrecoverable from those: the before-state
//! is a function of the after-state on a machine with one inbound edge per state,
//! and the boundary is on the row.
//!
//! What they do **not** give is the second half of `dod-audit`'s storage clause —
//! *append-only, tamper-evident*. The window row is protected by
//! `trg_pricing_price_window_append_only`'s column whitelist and not by a hash
//! chain, so the flip is durable and unattributable but not tamper-evident. That is
//! the residue, and it is stated rather than deferred, because deferring it would
//! be filing something as owed to a later group that **cannot be written today at
//! all**:
//!
//! * `AuditAction` has no `activate`/`expire` token, and the design set's own
//!   roster closes in both directions — a token with no writer is not declared, and
//!   **no writer without a token** (`05-governance.md` §6). This job would be a
//!   writer with no token.
//! * `subject_kind` includes `window` in the design set, while `chain_id`'s
//!   aggregate roster omits it in **both** places that roster is stated
//!   (`01-foundation.md` §3.7 and `05-governance.md` §6: *plan, overlay, payer,
//!   policy, bulk operation*). A window audit row therefore has a declared subject
//!   kind and no declared chain to hash into.
//!
//! So what is owed, and to whom: **two declarations from the design set's owner** —
//! an `action` token for a time-driven transition and a `chain_id` aggregate for a
//! window subject — before any group can write this row, G4 included. Reported as a
//! divergence. The phase plan puts `AuditSubjectKind::Window` with G4's writer and
//! does not consider the sweep's own case, which is why this paragraph exists.

use std::sync::Arc;

use std::collections::BTreeSet;

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::config::JobsConfig;
use crate::domain::audit::AuditStamp;
use crate::domain::error::DomainError;
use crate::domain::instant::format_rfc3339;
use crate::domain::scope_key::PlanId;
use crate::infra::storage::repo::window_repo::{DueBoundary, DueWindow};
use crate::infra::storage::repo::{
    NewOutboxEvent, PriceWindowTransitionPayload, outbox_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// The Warn alarm §7 names for a boundary that has stood uncrossed past the job
/// SLO. The string is the design set's, not this module's.
const ALARM_WINDOW_ACTIVATION_OVERDUE: &str = "pricing.window.activation_overdue";

/// How many due windows **one pass** reads, across both boundaries together.
///
/// §10 puts the sweep on `idx_pricing_price_window_due` *"in batches"*, and this
/// is the batch. A module constant rather than a config field for
/// `readmodel_warm`'s `FRONTIER_SCAN_LIMIT` reason: it bounds a read into memory
/// and no operator has a decision to make about it. Past the bound the tail is
/// not swept **this** pass — it is swept by the next one, with its boundaries
/// older, which is precisely what the overdue alarm is measured on. So a
/// saturated page degrades into a late flip that says so, never into a flip that
/// never happens.
///
/// # Per pass, and **not** per boundary
///
/// Applied per boundary it is two independent capped reads, merged and sorted, and a
/// saturated page then degrades into something worse than a late flip: a changeover
/// **split in half**. At one instant a predecessor ends and a successor begins; with
/// enough older expiries to fill the expiry page the predecessor's expiry falls
/// outside it while the successor's activation sits in an activation page nowhere
/// near its own cap, so the pass emits `PriceWindowActivated` and no
/// `PriceWindowExpired`, and a consumer reads a key held by two `active` windows at
/// once. [`sort_key`] cannot repair that: the losing row is not in the vector.
/// §10's own load figure makes it reachable — *"2N across a repricing run"*, so a
/// run over 600 rows produces ~1200 windows against a 1000-row bound.
///
/// One budget across the pass closes it, and the order the budget is spent in is
/// the guarantee rather than a preference: **expiries are read first** and
/// activations get what is left ([`activation_budget`]). An activation is therefore
/// read only when the expiry read came back short of its bound — and a capped read
/// that comes back short returned *everything* that matched, so every expiry due at
/// this instant, including any paired with a read activation, is in hand. The
/// invariant is "no activation without every expiry", and it holds by arithmetic
/// rather than by there being enough room.
///
/// What it costs, said plainly: a persistent expiry backlog **starves
/// activations**, one page per tick until it drains. That is the right direction
/// twice over — it is the same priority [`boundary_rank`] gives a changeover, so
/// the pass's budget and its sort now state one rule; and a starved activation ages
/// into the overdue alarm, which is a signal, while a split changeover is a
/// consumer resolving two prices for one key and says nothing at all.
const DUE_WINDOWS_PER_PASS: u64 = 1_000;

/// The actor a time-driven flip is stamped with: **nobody**.
///
/// The nil uuid, deliberately. Nil rather than an invented "system principal":
/// `inst-au-pii` makes the audit actor a **pseudonymous principal id**, and a uuid
/// minted here would be a principal that does not exist, indistinguishable in a
/// 7-year store from one that does.
///
/// It is safe today because it reaches no column, and that is a fact about the
/// signature rather than a promise: `window_repo::transition`'s stamp parameter is
/// spelled `_stamp`, so nothing on this path can persist it.
///
/// **It is not a tripwire, and it used to claim to be one.** The claim was that a
/// later group making this path write an audit record would find the nil actor
/// "loudly wrong instead of plausibly right". Nothing enforces that:
/// `pricing_audit_log.actor_principal_id` has no CHECK — `pricing_audit_log`'s only
/// CHECKs are `entry_kind`, `rollup` and `seq`, which `tests/postgres_migrations.rs`
/// confirms — so a nil uuid inserts cleanly and reads as an ordinary pseudonymous
/// id forever after. Rather than manufacture a refusal no decision names, the
/// obligation is written down where the next group will be standing: **a group that
/// spells this parameter `stamp` owes an actor of its own**, and owes it before the
/// row, not after.
const SYSTEM_ACTOR: Uuid = Uuid::nil();

/// What one pass did.
///
/// Returned rather than only logged, for [`SweepReport`](super::readmodel_warm::SweepReport)'s
/// reason: the suite asserts the pass's behaviour instead of scraping its output.
///
/// **Flat counters, not a per-tenant map**, which is a divergence from the phase
/// plan's sketch (*"activated, expired, overdue counts per tenant"*) and is
/// deliberate. A map would be a per-tenant report nothing reads — the sweep's
/// per-tenant facts are already in the log lines and in the rows it wrote — while
/// what a test and an operator both need is how much this pass moved. The
/// sibling job's report is the same shape for the same reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationReport {
    /// Windows whose boundary had arrived when the pass read them.
    pub windows_due: u64,
    /// Distinct `(tenant, plan)` pairs among them — the unit the work is ordered
    /// per.
    ///
    /// A **cardinality**, and nothing about the order: it is invariant under every
    /// permutation of the work, so no value of it could witness a pass that lost the
    /// ordering. That the ordering holds is proved by
    /// `tests/sqlite_window_activation.rs::one_plans_window_events_are_sequenced`
    /// against committed `seq` values, and by [`sort_key`]'s own unit tests.
    pub plans_seen: u64,
    /// `scheduled → active` flips that committed, each with its event.
    pub activated: u64,
    /// `active → expired` flips that committed, each with its event.
    pub expired: u64,
    /// Due windows whose transaction refused. The window stays due, so the next
    /// pass re-drives it and its boundary ages into
    /// [`ALARM_WINDOW_ACTIVATION_OVERDUE`].
    pub failed: u64,
    /// Due windows whose boundary was already older than the job SLO — the Warn
    /// alarm's count.
    pub overdue: u64,
    /// The oldest of them, as `(tenant, boundary instant, window)`.
    ///
    /// Carried because the alarm is raised once for the pass and an operator still
    /// needs one window to look at: the count says how wide the stall is, this says
    /// where it started. `None` whenever `overdue` is zero.
    pub oldest_overdue: Option<(Uuid, OffsetDateTime, Uuid)>,
}

/// The UTC window activation/expiry sweep: one pass, cross-tenant.
pub struct WindowActivationJob {
    db: DBProvider<DbError>,
    jobs: JobsConfig,
    /// The alarm plane (D-238), and the **third** ticker to get one.
    ///
    /// `pricing.window.activation_overdue` is §7's only window-plane alarm, and
    /// its text is "the lease singleton is stalled" — the signal that this whole
    /// plane has stopped advancing. A bare `tracing::error!` reaches
    /// `pricing_alarm_total` on no label and therefore
    /// reached no alerting rule: the estate could see every Info this gear
    /// raises and not the one that says the gear stopped working.
    ///
    /// The argument for a bare log is that this gear has no metrics or alarm
    /// facility; Slice 4 opened the plane, so it does. D-238 records the same
    /// defect closed twice in `readmodel_warm` and is filed BUILT, which is what
    /// lets a third instance sit here unlooked-for: a register entry saying a
    /// class is closed is not a census of its sites.
    metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
}

impl WindowActivationJob {
    /// Build the job over one database provider and the validated cadences.
    #[must_use]
    pub fn new(db: DBProvider<DbError>, jobs: JobsConfig) -> Self {
        Self {
            db,
            jobs,
            metrics: Arc::new(crate::domain::ports::metrics::NoopPricingMetrics),
        }
    }

    /// Attach the alarm plane — `readmodel_warm::with_metrics`' seam verbatim.
    ///
    /// A separate builder rather than a `new` parameter for its reason: every
    /// unit test constructing this job would otherwise have to name a sink it
    /// does not assert on, and the default is a `Noop` that keeps the log line
    /// working on its own.
    #[must_use]
    pub fn with_metrics(
        mut self,
        metrics: Arc<dyn crate::domain::ports::metrics::PricingMetricsPort>,
    ) -> Self {
        self.metrics = metrics;
        self
    }

    /// Run one pass at `now`.
    ///
    /// Read under [`AccessScope::allow_all`] — one sweep is one pass over every
    /// tenant — and **narrowed to `AccessScope::for_tenant` before every write**,
    /// which is the rule `crate::infra::jobs` states.
    ///
    /// # Ordering, and why it is the store's sequence rather than the scheduler's
    ///
    /// The work is sorted by `(tenant_id, plan_id, boundary instant, boundary,
    /// window_id)` and each flip is committed with its event, so the `seq` the
    /// outbox assigns within a plan is the order the transitions happened in.
    /// Two clauses of that key are not decoration:
    ///
    /// * **the boundary instant**, because a backlog must drain in the order it
    ///   accumulated;
    /// * **the boundary**, because an expiry and an activation on one instant is
    ///   a *changeover*, and the predecessor's end must reach a consumer before
    ///   the successor's start. `Expiry` sorts before `Activation` for that
    ///   reason and no other. Dropped from the key, the pair falls through to the
    ///   `window_id`, and one plan's changeover arrives as "the successor took
    ///   effect" followed by "the predecessor is over" for every pair whose
    ///   successor happens to carry the lower id.
    ///
    /// The two reads happen to run expiries-first as well, and that is **not** what
    /// makes the order right: `sort_unstable_by_key` does not preserve input order,
    /// so the read order reaches the consumer through nothing. The reads are in that
    /// order for the budget's sake (see [`DUE_WINDOWS_PER_PASS`]), which is about
    /// which rows are *read at all*; the sort is about the order the ones that were
    /// read commit in. Two rules, one direction, and neither is the other's proof.
    ///
    /// # Errors
    /// [`DomainError`] only when the pass cannot start — the connection or one of
    /// the two due reads failing. A per-window fault is isolated: it is counted,
    /// logged, and the window stays due for the next tick.
    pub async fn run(&self, now: OffsetDateTime) -> Result<ActivationReport, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: window sweep: {e}")))?;

        // Expiries first, and the activation read takes only what they left: see
        // `DUE_WINDOWS_PER_PASS` for why the order this budget is spent in is what
        // stops a saturated page from splitting a changeover.
        let mut due = self
            .read_due(&conn, DueBoundary::Expiry, now, DUE_WINDOWS_PER_PASS)
            .await?;
        let budget = activation_budget(due.len());
        if budget > 0 {
            due.extend(
                self.read_due(&conn, DueBoundary::Activation, now, budget)
                    .await?,
            );
        }
        due.sort_unstable_by_key(sort_key);

        let plans: BTreeSet<(Uuid, PlanId)> =
            due.iter().map(|w| (w.tenant_id, w.plan_id)).collect();
        let mut report = ActivationReport {
            windows_due: as_count(due.len()),
            plans_seen: as_count(plans.len()),
            ..ActivationReport::default()
        };

        for window in &due {
            self.observe_overdue(window, now, &mut report);
            self.flip(window, now, &mut report).await;
        }
        self.raise_overdue(&report, now);
        Ok(report)
    }

    /// One boundary's due page, or the failure that ends the pass.
    ///
    /// A read failure is **not** isolated the way a per-window fault is: a pass
    /// that could not read cannot know what it did not do, and reporting zero due
    /// windows would be a pass claiming a quiet store.
    async fn read_due(
        &self,
        conn: &impl DBRunner,
        boundary: DueBoundary,
        now: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<DueWindow>, DomainError> {
        window_repo::list_due(conn, &AccessScope::allow_all(), boundary, now, limit)
            .await
            .map_err(|e| repo_failure(&e))
    }

    /// Flip one window and enqueue its event, in **one** transaction.
    ///
    /// One transaction because §7 orders these events per `(tenant, plan)` and an
    /// event that could commit without its flip — or a flip without its event —
    /// would order a sequence against nothing: a consumer would be told a window
    /// took effect that had not, or resolve a key whose change it was never told
    /// about. That is what this closure buys and it buys it **structurally**: both
    /// writes are inside it, so no interleaving of failures can separate them. It is
    /// not what the race test demonstrates — see the module doc's third mechanism,
    /// where the two guarantees are kept apart.
    ///
    /// The transition instant is the **boundary**, not `now`
    /// ([`DueWindow::at`]): §4 puts the transition where the condition became
    /// true, so `activated_at` is `effectiveFrom` and `expired_at` is
    /// `effectiveTo`, for every window this sweep touches and however late it ran.
    ///
    /// **Where the lag is visible is `pricing_outbox.enqueued_at`**, which is `now`
    /// — the event row is the record of when the flip was performed, and
    /// `outbox_repo`'s doc says so. It is *not* visible in the window row:
    /// `transition`'s stamp parameter is spelled `_stamp`, so `stamp.recorded_at`
    /// reaches no column, and a column that always equals `effective_from` carries
    /// nothing the row does not already say. The two instants are worth keeping
    /// apart in the
    /// interface's contract (`transition`'s doc gives that argument); what they do
    /// not do is leave a trace of the delay on this table.
    ///
    /// A refusal is logged at **warn** and counted. It covers two cases with one
    /// sentence on purpose: a peer replica that got there first (the dedup key
    /// refusing the second event, which is the mechanism working) and a store
    /// that would not take the write. They are told apart by what happens next
    /// rather than by their log level — the first leaves the window flipped, so
    /// no later pass sees it; the second leaves it due, ageing into the overdue
    /// alarm. Splitting the levels would mean pre-reading the outbox to decide
    /// which one this is, which is a query racing the insert it is asking about.
    async fn flip(&self, window: &DueWindow, now: OffsetDateTime, report: &mut ActivationReport) {
        let scope = AccessScope::for_tenant(window.tenant_id);
        let payload = PriceWindowTransitionPayload {
            window_id: window.window_id,
            plan_id: window.plan_id,
            price_id: window.price_id,
            effective_from: window.effective_from,
            effective_to: window.effective_to,
            // Minted per transition, for the reason `readmodel_warm`'s degraded
            // emission gives: D-178 clause (2) binds a correlation to the
            // operator call, and a time-driven flip has none — no request asked
            // for it and the schedule that did is long committed. Per flip rather
            // than per pass, so two windows crossing one instant carry two values
            // and nothing claims they were one act.
            correlation_id: Uuid::now_v7(),
        };
        let event = match window.boundary {
            DueBoundary::Activation => {
                NewOutboxEvent::price_window_activated(window.tenant_id, &payload, now)
            }
            DueBoundary::Expiry => {
                NewOutboxEvent::price_window_expired(window.tenant_id, &payload, now)
            }
        };
        let (tenant_id, window_id, to, at) = (
            window.tenant_id,
            window.window_id,
            window.boundary.target_state(),
            window.at,
        );
        let stamp = AuditStamp {
            actor_principal_id: SYSTEM_ACTOR,
            recorded_at: now,
            correlation_id: payload.correlation_id,
        };

        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<(), DomainError, _>(move |txn| {
                Box::pin(async move {
                    window_repo::transition(txn, &scope, tenant_id, window_id, to, at, stamp)
                        .await
                        .map_err(|e| repo_failure(&e))?;
                    outbox_repo::enqueue(txn, &scope, event)
                        .await
                        .map_err(|e| repo_failure(&e))
                        .map(|_| ())
                })
            })
            .await;

        match outcome {
            Ok(()) => match window.boundary {
                DueBoundary::Activation => report.activated += 1,
                DueBoundary::Expiry => report.expired += 1,
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    tenant_id = %window.tenant_id,
                    window_id = %window.window_id,
                    boundary = ?window.boundary,
                    "bss-pricing: a due window did not transition; either a peer's sweep committed \
                     the same flip first, or the store refused it and the window stays due for the \
                     next tick"
                );
                report.failed += 1;
            }
        }
    }

    /// Raise `pricing.window.activation_overdue` for a boundary the pass found
    /// older than the job SLO.
    ///
    /// See the module doc for why the condition is read off the due set rather
    /// than off what the pass failed to flip, and for the age threshold's own
    /// argument.
    fn observe_overdue(
        &self,
        window: &DueWindow,
        now: OffsetDateTime,
        report: &mut ActivationReport,
    ) {
        let threshold = match time::Duration::try_from(self.jobs.window_activation_overdue_after())
        {
            Ok(threshold) => threshold,
            Err(e) => {
                crate::infra::jobs::unconvertible_threshold(
                    e,
                    "window_activation_overdue_after",
                    ALARM_WINDOW_ACTIVATION_OVERDUE,
                );
                return;
            }
        };
        let late = now - window.at;
        if late < threshold {
            return;
        }
        // **Counted here, raised once per pass.** The alarm §7 declares is one
        // fact about one singleton — "the lease singleton is stalled" — and raising
        // it per row made the counter's rate a function of backlog size rather than
        // of the outage: a healthy pass draining a large scheduled batch fired a
        // threshold tuned to one stalled window, and a persistent refusal made the
        // error log unreadable for as long as it stood. Up to `DUE_WINDOWS_PER_PASS`
        // lines and increments a tick.
        //
        // The oldest boundary is kept because that is the one an operator acts on;
        // the count is what says how wide the stall is.
        report.overdue += 1;
        if report
            .oldest_overdue
            .is_none_or(|(_, at, _)| window.at < at)
        {
            report.oldest_overdue = Some((window.tenant_id, window.at, window.window_id));
        }
    }

    /// The pass's one overdue alarm, raised after the loop.
    ///
    /// Both a line and a counter, for `readmodel_warm`'s reason: the log carries
    /// what an operator needs to act on, and the alarm is what an alerting rule can
    /// match. D-238's own words — "a Critical that an operator can only find by
    /// grepping logs is the state the plane was opened to end".
    fn raise_overdue(&self, report: &ActivationReport, now: OffsetDateTime) {
        let Some((tenant_id, at, window_id)) = report.oldest_overdue else {
            return;
        };
        tracing::error!(
            alarm = ALARM_WINDOW_ACTIVATION_OVERDUE,
            overdue = report.overdue,
            oldest_tenant_id = %tenant_id,
            oldest_window_id = %window_id,
            oldest_boundary_at = %format_rfc3339(at),
            oldest_late_secs = (now - at).whole_seconds(),
            "bss-pricing: price window boundaries have stood uncrossed past the activation SLO; \
             the lease singleton is stalled or its flips are being refused"
        );
        self.metrics
            .alarm(crate::domain::ports::metrics::PricingAlarm::WindowActivationOverdue);
    }
}

/// The order one pass does its work in — see
/// [`WindowActivationJob::run`] for why each clause is there.
///
/// A free function so it is assertable without a database: the changeover order
/// is the one property of this key that a suite could otherwise only observe
/// through two committed outbox rows.
fn sort_key(window: &DueWindow) -> (Uuid, Uuid, OffsetDateTime, u8, Uuid) {
    (
        window.tenant_id,
        window.plan_id.get(),
        window.at,
        boundary_rank(window.boundary),
        window.window_id,
    )
}

/// An expiry sorts **before** an activation on one instant.
///
/// Spelled out rather than derived from [`DueBoundary`]'s declaration order,
/// because the ordering is a rule about changeovers and a derive would make it a
/// property of which variant somebody wrote first — the kind of dependency that
/// survives until the day the enum is tidied.
const fn boundary_rank(boundary: DueBoundary) -> u8 {
    match boundary {
        DueBoundary::Expiry => 0,
        DueBoundary::Activation => 1,
    }
}

/// What is left of [`DUE_WINDOWS_PER_PASS`] once the expiry read has taken its
/// share.
///
/// A free function for [`sort_key`]'s reason — the arithmetic is a rule about
/// changeovers and is assertable without a database. Saturating, so a page that
/// somehow came back over its own bound yields **zero** rather than wrapping to a
/// bound larger than the one it was given.
///
/// Zero is the load-bearing value: it is what a saturated expiry page produces, and
/// it is what makes "no activation is read unless every due expiry was" true rather
/// than likely.
fn activation_budget(expiries_read: usize) -> u64 {
    DUE_WINDOWS_PER_PASS.saturating_sub(as_count(expiries_read))
}

/// A count for the report, saturating rather than casting — `readmodel_warm`'s
/// `as_count`, for its reason.
fn as_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "window_activation_tests.rs"]
mod window_activation_tests;
