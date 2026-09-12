//! The `ReadModelProjector` — what turns a committed `CatalogVersion` into
//! something a consumer can pin.
//!
//! G5's publish commit deliberately stops one step short: it writes the flipped
//! revision, the published rows, a **pending** `pricing_catalog_version_ref`
//! carrying its subject (D-157), an undrained `PlanPublished` and an audit row,
//! and **nothing** in `pricing_read_model` or `pricing_pin_frontier`. It cannot
//! do otherwise — that table is keyed `catalog_version NOT NULL` and the commit
//! holds a handle, not a number. This module begins there: resolve the handle,
//! project the subject it published, write the delta warm, and advance the
//! frontier if that completed the version.
//!
//! It lives in `infra` rather than in `domain` because it holds an
//! `AccessScope` and repositories, which dylint DE0301 forbids inside
//! `src/domain/**` — the same reason [`crate::infra::publish`] is here, and the
//! sibling ledger puts `infra::posting::service` here. The projected payload's
//! **vocabulary and rendering** stay pure, in [`crate::domain::projection`].
//!
//! # Where the version comes from: a pull, not a push
//!
//! D-66 settled the emitter — `CatalogVersionPublished` is the **registry's**
//! event, tenant-wide, and deliberately not in this gear's frozen event set.
//! `gears/bss/products` is docs only, there is no event broker in this
//! repository at all (`config.events_enabled` defaults `false` and says why),
//! so an inbound port defined here would be a trait with neither producer nor
//! caller.
//!
//! The pull is already contracted. `CatalogVersionRegistryV1::committed_version`
//! resolves a pending ref to its committed version, and its own doc hands the
//! overdue decision to the caller "since only it knows how long the ref has
//! been outstanding" — which is this gear's sweep over
//! `pricing_catalog_version_ref.requested_at`. The port was written for a
//! reconciler, and this sweep is it.
//!
//! **When the registry gear lands, its event handler hooks to
//! [`ReadModelProjector::project_version`]** — this entry point, unchanged —
//! and the sweep stays as the backstop §3.6's own remediation posture already
//! asks for ("a `CatalogVersionPublished` batch that omits an expected pending
//! ref is treated the same; remediation is a registry re-request, never a
//! silent re-emit").
//!
//! # Three premises, each stated as a premise
//!
//! The [`crate::infra::publish`] posture, three times. Each is a place where
//! what is built and what the design set says are not the same thing, and each
//! is written here rather than reconciled by editing a document.
//!
//! ## 1. D-121's horizon is absent, and what holds it is that the set it
//! filters is empty
//!
//! D-121 projects "every price row of the plan whose window set intersects
//! `[projection_time - H,...)`", `H = 2 x` the longest billing cycle sold on
//! the plan.
//!
//! **The predicate has an operand.** The window store exists and
//! [`project_windows`] reads it, so a version this projector writes carries the
//! intervals, states and derived coverage end `inst-sg-surface` requires — which is
//! what predicate (1) needs *from the pin*, and is not the same thing as predicate
//! (1) being evaluable.
//!
//! **W6 is an input to sellability predicate (1), not the predicate.** §1.6's W6 is
//! a **term** — "the longest billing cycle sold on the key" — and predicate (1) is
//! "an **active** window covers `t` **with the D-80 coverage horizon**", a
//! conjunction over the frozen intervals, the reader's `t` and that term. Carrying
//! intervals, states and a coverage end supplies the first two operands and **not**
//! the term, so nothing here makes W6 evaluable and an earlier draft of this
//! paragraph claiming otherwise by apposition was wrong rather than loose.
//!
//! **The term has a producer**: `domain::coverage::longest_cycle_sold` (and
//! `longest_cycle_sold_on`, the same implementation over the two facts it reads),
//! W6, built with G3's coverage rules. The ownership chain below is kept for the one
//! owed input two documents named two owners for — `SellabilitySurface`'s group here,
//! G3 in `window_repo`'s deferred entry:
//!
//! * **G3 builds the cycle set.** It owns `CoverageChecker` and the D-80 horizon,
//!   which is the same term under a different margin. **Built.**
//! * **G4 wires it into the `effectiveTo` path.** §6 requires the D-04 bound
//!   enforced "at cutover and on every `effectiveTo` adjustment". **Built** as
//!   `infra::window::refuse_horizon_uncovered` rather than inside
//!   `window_repo::adjust_effective_to`: the repository layer holds one window and
//!   its key, and the bound's operands are the plan shape's margin and the price
//!   row's horizon. That function's own doc carries the argument. **What it covers
//!   is `infra::window::mutate_in`'s three acts and not every path that writes a
//!   window** — `window_repo`'s roster names the four that bypass it and says
//!   which are argued shut; the cutover half of §6's sentence is one of them, held
//!   by construction rather than by this rule.
//! * **G5 consumes it in predicate (1)**, which is where the D-80 margin is read.
//!   Still owed: what this projector freezes is intervals, states and a coverage
//!   end, and the horizon predicate is the sellability surface's to evaluate.
//!
//! **The trap, recorded because the name is the trap.** W6 is *named* "the longest
//! billing cycle sold on the **key**" and is *defined* per **plan**: the longest
//! `frequency` among the plan's **recurring** rows on the key's
//! `(currency, region)`. §1.6 says so in as many words and gives the reason — a
//! `usage`, `one_time` or `one_time_setup` key carries no `frequency` at all, so the
//! literal per-key reading is undefined on most keys of a hybrid — and matches it to
//! D-121's own `H`. It is **zero** on a plan with no recurring part, where the
//! horizon predicate reduces to "an active window covers `t`". Whoever builds it
//! from the name alone builds the per-key reading and gets `None` where the answer
//! is a number, or a number where the answer is zero.
//!
//! **The horizon itself is still absent. The premise that made projecting
//! without it safe is gone, and nothing has replaced it**. What the bound
//! buys is stated in D-121's own words: "a delta is O(live keys x windows within `H`), never
//! O(the plan's accumulated history)". A plan's accumulated history is its `superseded` price
//! rows, and `published -> superseded` has exactly two sanctioned producers — the D-88
//! supersession unit and the D-100 cutover.
//!
//! **Three mounted routes reach the flip.** Claiming otherwise is the standing
//! error on this paragraph — first that nothing produces the transition at all,
//! then that no *surface* reaches it — so the producers are tabulated with the
//! path from route to writer rather than asserted:
//!
//!    | Route | merged at | reaches |
//!    |---|---|---|
//!    | `POST /plans/{planId}/supersessions` | `module.rs`, `supersessions::router` | `supersede_in` -> [`crate::infra::supersession::commit_supersession`] -> `price_repo::commit_supersession_rows` |
//!    | `POST /plans/{planId}/cutovers` | `module.rs`, `cutovers::router` | `cutover_in` -> [`crate::infra::cutover::commit_cutover`] |
//!    | `POST /repricing-runs` | `module.rs`, `repricing_runs::router` | `apply_run_in` -> `commit_supersession` per journal row |
//!
//! **The premise is gone and the horizon is unpaid.** The set this projector reads is
//! [`crate::domain::projection::PROJECTED_ROW_STATES`] — `published` **and**
//! `superseded` — with no horizon predicate, and [`project_windows`] draws its
//! window set from those same rows with `expired` included. Every plan delta
//! therefore re-projects the plan's entire accumulated supersession history into
//! an INSERT-only store on a seven-year horizon whose contract is that a
//! completed version never changes. That is exactly the growth D-121's bound
//! exists to stop.
//!
//! **What bounds the set today: nothing.** `SoftSizeCaps::max_price_rows_per_plan`
//! does not, and it is wrong three ways to read it as a cap of 500 here. It is
//! **advisory** — it raises
//! `PLAN_SIZE_SOFT_CAP_EXCEEDED` as a *warning* whose own message says "the
//! publish proceeds" — its subject is the publish's candidate row set
//! [`CANDIDATE_ROW_STATES`](crate::infra::publish::CANDIDATE_ROW_STATES), which
//! is `[published, draft]`, and it is a cap on **rows per plan revision**, not on
//! the history those rows accumulate. Superseded rows are outside the cap, outside
//! the states it counts, and unbounded by anything else in this crate: a plan
//! repriced monthly grows its delta payload monthly and forever. The real bound
//! is the number of supersessions a tenant performs, which is a rate rather than
//! a limit.
//!
//! **The predicate now has both operands it lacked.**
//! `domain::coverage::longest_cycle_sold` is `H`'s input and is built (see the
//! trap above for the reading it must take — per **plan**, not per key), and
//! [`project_windows`] supplies the interval set the predicate ranges over.
//! Building it is a design act rather than a repair, which is why this note
//! records the debt instead of discharging it: the filter decides what a *frozen*
//! version carries, rating rates past instants against exactly those payloads, and
//! choosing `H` chooses how far back a resolution can still succeed. That is a
//! decision the register owes an entry, not something a doc correction may smuggle in.
//!
//! **Why this is a cost and not yet a fault.** No wrong price results. The one
//! live consumer that selects a row by market filters on state —
//! `api::rest::preview` asks `row["lifecycleState"] == published` — so a
//! superseded predecessor can never be quoted. What it costs is payload size on a
//! monotonically growing store, deserialized per `/preview` and per
//! window-sellability request.
//!
//! The cost of projecting anyway is still real, and it is smaller by one fact than
//! it was: the per-fact list in [`crate::domain::projection`] now records the
//! window facts as **present**, and predicate (5)'s GA-gate flags and predicate
//! (6)'s registry `sellable` flag as the two that remain absent — so a version
//! this projector writes answers four of six rather than three.
//!
//! **The payload has readers.** Two mounted read surfaces resolve a frozen delta
//! and walk it:
//!
//! - `api::rest::preview` calls `read_model_repo::delta_at` at the tenant's
//!   frontier and then walks `payload["prices"]` for the requested market;
//! - `api::rest::windows`' sellability report calls the same `delta_at` and then
//!   [`read_model_repo::sellability_facts`](crate::infra::storage::repo::read_model_repo::sellability_facts),
//!   which reads every member of the same array.
//!
//! So the payload is not write-only, the growth above is paid on every one of
//! those reads, and a reader deciding how much this projector may safely freeze
//! must not take this module's word that nobody is looking.
//!
//! ## 2. D-136's advance strands a version that completes out of order
//!
//! Read literally, the frontier advances only in the transaction completing
//! "the frontier's **next** version in order". §4.4's own D-114 worked example
//! is `V5` degraded and `V6` fully warm: when `V5` finally completes the
//! frontier moves to `V5` — and `V6`, already complete, will never see another
//! completion to advance on. The frontier would stand at `V5` forever with a
//! complete `V6` behind it.
//!
//! So [`advance_frontier`] **walks forward** after an advance, moving the
//! watermark through every immediately-following version that is already fully
//! warm. That is strictly within "only ever forward" and "never past a gap" —
//! what it is not is the literal sentence. **Reported.**
//!
//! ## 3. §4.4's "current revision" is the wrong source, and the ref row carries
//! what is right
//!
//! If revision 3 publishes into `V5` and revision 4 publishes into `V6` before
//! `V5` warms, projecting `V5` from the plan's *current* revision freezes
//! revision 4's content into `V5`'s delta. §4.4 defends the re-drive against
//! **draft** leakage and says nothing at all about a later **published**
//! revision — and the window is up to D-47's five-minute batching maximum
//! rather than the 5s warm SLO, so this is reachable rather than theoretical.
//! Permanently, too: a delta is INSERT-only on the seven-year horizon, in a
//! store whose contract is that a completed version never changes.
//!
//! So the commit pins **the revision it judged and the lifecycle state that
//! judgement produced** on the ref row, and the projector reads those. Putting the
//! revision there does **not** contradict D-86/D-91's subject typing or invent a
//! column: D-86/D-91 key `pricing_read_model`, which is untouched, and a revision is
//! a property of the **publish unit**, exactly as D-157's subject columns are.
//!
//! D-128's substance is untouched and is what makes a pinned revision safe:
//! `(plan_id, revision)` is the same row whether it stands `published` or
//! `retired`, and retirement opens no revision — it is a publish unit of its
//! own, which pins that same number with its own state and its own version.
//! That is also why the **state** is pinned and not read live: read live it
//! admits `superseded`, a third value D-128 does not contemplate for a
//! projected subject and that `load_current` could never return, which a
//! consumer coding sellability predicate (4) as "is published" reads as
//! unsellable.
//!
//! **Both columns are reported as divergences.** §3.7 lists neither, and
//! §4.4's current-revision rule is what they contradict.
//!
//! # What is deliberately not here
//!
//! **No audit row.** No document requires one for a projection, and
//! `pricing_audit_log` is the actor/before-after trail of a **mutation**
//! (`inst-au-complete`): a projection has no actor and changes no truth. The
//! absence is stated so it is not read as an oversight.
//!
//! **No producer for `DomainError::ReadModelUnavailable`.** It is the **read**
//! path's fail-closed answer, and the read path does not exist.
//!
//! **No wire code for anything refused here.** The projector has no caller a
//! client can reach — it is driven by a background sweep — so a refusal has
//! nobody to report to, which is D-146's own line about the pin frontier.

use bss_pricing_sdk::CatalogVersion;

use toolkit_db::secure::{AccessScope, DBRunner};
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::price_record::PriceRecord;
use crate::domain::projection::{
    MembershipSubjectDelta, OverlayIndexDelta, OverlayIndexEntry, OverlaySubjectDelta,
    PROJECTED_ROW_STATES, PROJECTED_WINDOW_STATES, PlanSubjectDelta, RowResolutionProjection,
};
use crate::domain::read_model::{OverlayIndexShard, SubjectKind, SubjectRef};
use crate::domain::scope_key::PlanId;
use crate::domain::tax_display::{TAX_ENGINE_GA, is_not_sellable_ga};
use crate::domain::window::{self, KeyWindows, WindowInterval};
use crate::infra::storage::repo::catalog_version_ref_repo::RefIdentity;
use crate::infra::storage::repo::{
    NewDelta, PendingVersionRow, catalog_version_ref_repo, group_membership_repo, overlay_repo,
    pin_frontier_repo, plan_repo, plan_shape_repo, price_repo, read_model_repo, window_repo,
};
use crate::infra::storage::repo_failure;
use time::OffsetDateTime;

/// Whether the pass calling the projector **could have seen** the whole of one
/// tenant's pending set (D-163 clause 2).
///
/// §4.4's pin-eligibility counts "**every** subject row that version projects",
/// which is decidable only if the version's subject set is closed. Clause (1)
/// closes it — a batch commits atomically into one version, so any one committed
/// ref of `V` closes `V`'s set — but *this gear* learns each ref's version by
/// asking the registry **per ref**, so its knowledge of the set is only as good
/// as the pass that gathered it. Two ordinary conditions leave a pass holding
/// part of a set, and in both the version reads complete, the frontier advances,
/// and the straggler then arrives at an already-pinnable version.
///
/// A pass that could not have seen the whole set resolves and warms exactly as
/// usual and decides **no completion**. The frontier lags, which is the safe
/// direction, and it is not silent: `pricing.readmodel.pin_eligibility_overdue`
/// (D-166 clause 5) fires on exactly that state.
///
/// **`Ok(None)` from the registry is not one of the two conditions**, and the
/// distinction is load-bearing rather than pedantic. "Not committed yet" is a
/// successful answer: under clause (1) a ref the registry has not committed
/// cannot belong to a version that already has a committed ref, so it says
/// nothing about that version's set. A registry **error** says the pass does not
/// know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PassCoverage {
    /// Every pending ref of the tenant was read, and the registry answered for
    /// each of them.
    Whole,
    /// The per-tenant scan filled its bound, so refs beyond it were not read and
    /// a version's refs may straddle the page.
    ScanSaturated,
    /// The registry **failed** to answer for at least one ref of this tenant, so
    /// a sibling of some version is of unknown version.
    RefUnresolved,
}

impl PassCoverage {
    /// May a pass with this coverage decide a version complete?
    #[must_use]
    pub const fn may_decide_completion(self) -> bool {
        matches!(self, Self::Whole)
    }

    /// Why not, for a log line. `None` when it may.
    #[must_use]
    pub const fn deferral_reason(self) -> Option<&'static str> {
        match self {
            Self::Whole => None,
            Self::ScanSaturated => Some("the per-tenant pending scan filled its bound"),
            Self::RefUnresolved => Some("a ref of this tenant could not be resolved"),
        }
    }
}

/// What one pass over one version did — the sweep's input for the degraded
/// observation, and what the suite asserts against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionReport {
    /// Subjects this version publishes, per its ref rows (D-157).
    pub subjects: usize,
    /// Subjects this call projected. Zero on a re-drive that had nothing left.
    pub projected: usize,
    /// Subjects whose own transaction refused. Their refs stay pending, so the
    /// next tick re-drives them and their age eventually alarms.
    pub failed: usize,
    /// Whether every subject of the version is warm at the end of the call.
    pub complete: bool,
    /// Where the frontier stands if this call moved it. `None` means it did
    /// not, which on an incomplete version is the normal answer and on a
    /// complete one means an earlier version is still outstanding — the D-114
    /// prefix, holding.
    pub frontier_advanced_to: Option<CatalogVersion>,
}

/// The projector: one entry point, over one database provider.
#[derive(Clone)]
pub struct ReadModelProjector {
    db: DBProvider<DbError>,
}

impl ReadModelProjector {
    /// Build over one database provider.
    #[must_use]
    pub fn new(db: DBProvider<DbError>) -> Self {
        Self { db }
    }

    /// Make every subject of one committed version resolvable, and advance the
    /// frontier if that completes it.
    ///
    /// `newly_committed` are the refs the sweep has just resolved to this
    /// version and which are therefore still `pending` in storage; they are
    /// finalized here, inside the transaction that projects them, so a crash
    /// between the two cannot leave a committed ref nothing ever projected.
    /// On a pure re-drive it is empty and the subject set comes entirely from
    /// the refs an earlier pass finalized.
    ///
    /// # The sequence, and why each move sits where it does
    ///
    /// 1. **The subject set** is the version's ref rows — the already-finalized
    ///    ones plus `newly_committed`. D-157 is why that is the only durable
    ///    answer: with no subject on the ref row there is no path at all from a
    ///    handle back to what it published.
    /// 2. **Subtract what is already warm.** On a first warm the difference is
    ///    everything; on a re-drive it is exactly what failed last time. **The
    ///    re-drive is this subtraction**, not a second mechanism, and §4.4's
    ///    unbounded degraded re-drive is the sweep simply arriving again.
    /// 3. **One transaction per outstanding subject**: finalize its ref,
    ///    project it from truth, write the delta warm, then re-read the
    ///    version's warm set — and if it is now complete **and** this version
    ///    is the frontier's next in order, advance, in that same transaction.
    ///    That is D-136 executed literally: the transaction setting the last
    ///    outstanding marker is the one that advances.
    ///
    /// Per subject rather than per version because D-91 projects a version's
    /// subjects independently: one subject whose truth rows are unreadable must
    /// not roll back the subjects that projected cleanly, or a version with one
    /// bad plan could never complete any of its others.
    ///
    /// `coverage` gates **only** the completeness decision, in both the places
    /// one is made — the report below and the advance inside `project_one`. A
    /// pass that could not have seen the whole subject set still resolves and
    /// warms everything it holds; what it does not do is claim a count over a
    /// set it knows is partial. See [`PassCoverage`].
    ///
    /// # Errors
    /// [`DomainError::Internal`] when a ref names a subject kind this gear
    /// cannot have written, when its reference is not the identifier that kind
    /// is keyed by, when a plan subject has no current revision, or on any
    /// storage failure. There is no other class: every input here comes from
    /// this gear's own tables or from the registry, so a refusal is an
    /// invariant breach and never a caller mistake.
    pub async fn project_version(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        catalog_version: CatalogVersion,
        newly_committed: &[PendingVersionRow],
        coverage: PassCoverage,
        now: OffsetDateTime,
    ) -> Result<VersionReport, DomainError> {
        let conn = self
            .db
            .conn()
            .map_err(|e| DomainError::Internal(format!("bss-pricing: projector: {e}")))?;
        let subjects =
            version_subjects(&conn, scope, tenant_id, catalog_version, newly_committed).await?;
        let warm = read_model_repo::warm_subjects_at(&conn, scope, tenant_id, catalog_version)
            .await
            .map_err(|e| repo_failure(&e))?;

        let outstanding: Vec<PendingVersionRow> = outstanding_subjects(&subjects, &warm);
        let mut projected = 0_usize;
        let mut failed = 0_usize;
        let mut advanced_to = None;
        for subject in outstanding {
            // Per subject, and a fault here does **not** abandon the version.
            // D-91 projects a version's subjects independently, so one plan
            // whose truth rows are unreadable must not stop the others - and a
            // return here would additionally discard an advance an earlier
            // subject's transaction has already committed, which the report
            // would then deny having made.
            match self
                .project_one(
                    scope,
                    tenant_id,
                    &VersionPass {
                        catalog_version,
                        subjects: &subjects,
                        coverage,
                        now,
                    },
                    &subject,
                )
                .await
            {
                Ok(moved) => {
                    projected += 1;
                    if moved.is_some() {
                        advanced_to = moved;
                    }
                }
                Err(e) => {
                    failed += 1;
                    tracing::error!(
                        error = %e,
                        tenant_id = %tenant_id,
                        catalog_version = catalog_version.get(),
                        pending_ref = %subject.pending_ref,
                        "bss-pricing: projecting one subject failed; its ref stays pending"
                    );
                }
            }
        }

        let warm = read_model_repo::warm_subjects_at(&conn, scope, tenant_id, catalog_version)
            .await
            .map_err(|e| repo_failure(&e))?;
        // The completeness bound. A warm set with nothing outstanding is
        // evidence of completeness only over a subject set the pass could have
        // seen whole; over a partial one it is a count taken while the set was
        // still being added to (D-163 clause 2).
        let warm_and_whole =
            !subjects.is_empty() && outstanding_subjects(&subjects, &warm).is_empty();
        log_deferred_completion(tenant_id, catalog_version, coverage, warm_and_whole);
        Ok(VersionReport {
            subjects: subjects.len(),
            projected,
            failed,
            complete: warm_and_whole && coverage.may_decide_completion(),
            frontier_advanced_to: advanced_to,
        })
    }

    /// One subject, one transaction: finalize, project, warm, and advance if
    /// that was the last outstanding marker.
    async fn project_one(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        pass: &VersionPass<'_>,
        subject: &PendingVersionRow,
    ) -> Result<Option<CatalogVersion>, DomainError> {
        let VersionPass {
            catalog_version,
            subjects,
            coverage,
            now,
        } = *pass;
        let scope = scope.clone();
        let subject = subject.clone();
        let siblings = subjects.to_vec();
        let (_, outcome) = self
            .db
            .db()
            .in_transaction::<Option<CatalogVersion>, DomainError, _>(move |txn| {
                Box::pin(async move {
                    let projected = subject_of(&subject)?;
                    refuse_projection_below_frontier(
                        txn,
                        &scope,
                        tenant_id,
                        catalog_version,
                        &subject.pending_ref,
                    )
                    .await?;
                    catalog_version_ref_repo::finalize(
                        txn,
                        &scope,
                        RefIdentity::of(&subject),
                        catalog_version,
                        now,
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // One transaction, one subject, whichever plane it is on.
                    // The two arms differ only in what they read and render;
                    // everything around them - the frontier refusal, the
                    // finalize, the completeness decision - is the same, which
                    // is what makes an overlay publish a publish rather than a
                    // second mechanism.
                    let (subject_ref, payload) = match projected {
                        ProjectedSubject::Plan {
                            plan_id,
                            revision,
                            lifecycle_state,
                        } => {
                            let delta = project_plan_subject(
                                txn,
                                &scope,
                                tenant_id,
                                plan_id,
                                revision,
                                lifecycle_state,
                            )
                            .await?;
                            (SubjectRef::Plan(plan_id.get()), delta.to_value())
                        }
                        ProjectedSubject::Overlay {
                            price_overlay_id,
                            revision,
                        } => {
                            let delta = project_overlay_subject(
                                txn,
                                &scope,
                                tenant_id,
                                price_overlay_id,
                                revision,
                            )
                            .await?;
                            (SubjectRef::PriceOverlay(price_overlay_id), delta.to_value())
                        }
                        ProjectedSubject::OverlayIndex { shard } => {
                            let delta = project_overlay_index_subject(
                                txn,
                                &scope,
                                tenant_id,
                                catalog_version,
                                &siblings,
                                shard.clone(),
                            )
                            .await?;
                            (SubjectRef::OverlayIndex(shard), delta.to_value())
                        }
                        ProjectedSubject::GroupMembership {
                            membership_id,
                            row_version,
                            effective_to,
                        } => {
                            let delta = project_membership_subject(
                                txn,
                                &scope,
                                tenant_id,
                                membership_id,
                                row_version,
                                effective_to,
                            )
                            .await?;
                            (SubjectRef::GroupMembership(membership_id), delta.to_value())
                        }
                    };
                    read_model_repo::project_subject(
                        txn,
                        &scope,
                        NewDelta {
                            tenant_id,
                            catalog_version,
                            subject: subject_ref,
                            payload,
                            projected_at: now,
                        },
                    )
                    .await
                    .map_err(|e| repo_failure(&e))?;

                    // The same bound as the report's, at the place where the
                    // decision has a consequence rather than a value: an
                    // advance is what makes a version pinnable, so a pass
                    // holding a partial subject set must not reach it.
                    if !coverage.may_decide_completion()
                        || !version_is_complete(txn, &scope, tenant_id, catalog_version, &siblings)
                            .await?
                    {
                        return Ok(None);
                    }
                    advance_frontier(txn, &scope, tenant_id, catalog_version, now).await
                })
            })
            .await;
        outcome.map_err(|err| {
            err.into_domain(|infra| {
                DomainError::Internal(format!("bss-pricing: projection transaction: {infra}"))
            })
        })
    }
}

/// What the pass holds about the version it is projecting.
///
/// One value rather than four parameters because the four are one fact — this
/// pass's whole knowledge of this version — and a per-subject transaction that
/// took the version without the coverage could advance a frontier over a subject
/// set the pass was still guessing at, which is exactly D-163 clause (2)'s defect.
#[derive(Clone, Copy)]
struct VersionPass<'a> {
    /// The version every subject here belongs to.
    catalog_version: CatalogVersion,
    /// Every subject the pass believes the version publishes — the already
    /// finalized refs plus the ones it is finalizing.
    subjects: &'a [PendingVersionRow],
    /// Whether the pass could have seen that set whole.
    coverage: PassCoverage,
    /// The pass's instant, for the finalize and the warm marker.
    now: OffsetDateTime,
}

/// Say out loud that a version reads warm and was not judged complete.
///
/// Silent deferral is how a lagging frontier becomes indistinguishable from a
/// stuck one, and the two have different remedies: this one needs the next pass,
/// that one needs an operator.
fn log_deferred_completion(
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
    coverage: PassCoverage,
    warm_and_whole: bool,
) {
    if !warm_and_whole {
        return;
    }
    if let Some(reason) = coverage.deferral_reason() {
        tracing::info!(
            tenant_id = %tenant_id,
            catalog_version = catalog_version.get(),
            reason,
            "bss-pricing: a version reads warm and this pass may not judge it complete; the \
             frontier lags until a pass that could have seen the whole subject set arrives"
        );
    }
}

/// The subject set of one version: the refs already finalized, plus the ones
/// this pass has just resolved and is about to finalize.
///
/// Deduplicated on the handle **and the subject**, because a re-drive re-offers
/// a ref the previous pass finalized and the two halves would otherwise both
/// name it.
///
/// **The subject is in that key since D-234**, and dropping it would silently
/// lose subjects rather than merely duplicate them: one handle now carries every
/// subject its unit projects — two or three on the overlay plane (D-112, D-133)
/// — so a dedup on `pending_ref` alone would admit the first row of a handle and
/// discard its siblings. The version would then read complete over a subject set
/// missing the very rows that make an overlay enumerable, and the frontier would
/// advance past it.
async fn version_subjects(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
    newly_committed: &[PendingVersionRow],
) -> Result<Vec<PendingVersionRow>, DomainError> {
    let mut subjects =
        catalog_version_ref_repo::list_at_version(runner, scope, tenant_id, catalog_version)
            .await
            .map_err(|e| repo_failure(&e))?;
    for candidate in newly_committed {
        if !subjects.iter().any(|known| {
            known.pending_ref == candidate.pending_ref
                && known.subject_kind == candidate.subject_kind
                && known.subject_ref == candidate.subject_ref
        }) {
            subjects.push(candidate.clone());
        }
    }
    Ok(subjects)
}

/// The subjects of a version that are not yet warm.
///
/// Pure, and extracted for it: this subtraction **is** the re-drive, so it is
/// the one piece of the projector worth asserting without a database.
fn outstanding_subjects(
    subjects: &[PendingVersionRow],
    warm: &[(SubjectKind, String)],
) -> Vec<PendingVersionRow> {
    subjects
        .iter()
        .filter(|subject| {
            !warm.iter().any(|(kind, reference)| {
                *kind == subject.subject_kind && *reference == subject.subject_ref
            })
        })
        .cloned()
        .collect()
}

/// What a ref row's subject columns name, resolved into something projectable.
///
/// **The refusal is what keeps a version honest.** A subject silently unprojected
/// holds its version incomplete, and therefore holds the frontier, forever with
/// nothing saying why — so an unanswerable kind is refused *by name* rather than
/// skipped.
///
/// **The reason a kind is unanswerable is a missing projector, never a missing
/// store.** Naming the store instead dates the refusal to the next migration: a
/// reader who checks "no store in this gear" by grepping for the table finds one
/// and reads the whole refusal as stale.
///
/// `GroupMembership`'s store is `pricing_group_membership`, written by
/// `group_membership_repo::enroll`/`end_membership`, and the projector's read of
/// it is [`project_membership_subject`]. That is the surface Tariffs reads to do
/// its own resolve-and-freeze (`ResolvedGroupFreezer`, D-30): this gear publishes
/// membership into the read model and stops there, exactly as
/// `MembershipSubjectDelta`'s doc states at length. **Every member of
/// `SubjectKind::ALL` is answerable.**
#[derive(Debug)]
enum ProjectedSubject {
    /// A plan, at the revision and lifecycle state its publish judged.
    Plan {
        /// Which plan.
        plan_id: PlanId,
        /// The revision the ref pinned.
        revision: u64,
        /// The state that revision was in when the publish judged it.
        lifecycle_state: LifecycleState,
    },
    /// An overlay document, at the revision its publish judged.
    ///
    /// **No lifecycle state, unlike the plan arm**, and the asymmetry is a
    /// decision rather than an omission. D-128 pins a plan's state because
    /// sellability predicate (4) reads it at the pin and the row's own state keeps
    /// moving (`published -> superseded` at the next revision's commit). An
    /// overlay revision's state is carried **inside the document** it freezes —
    /// `OverlayRevision::lifecycle_state`, read from the pinned revision row,
    /// which is itself immutable once published — so pinning it a second time on
    /// the ref would be two places for one fact to disagree.
    Overlay {
        /// Which overlay.
        price_overlay_id: Uuid,
        /// The revision the ref pinned.
        revision: u64,
    },
    /// One `overlay_index` shard, whose content is **derived** rather than
    /// pinned (D-112, D-133).
    ///
    /// No revision and no lifecycle state, because a shard is not a revisioned
    /// thing: it is the set of overlays live at this version, and what pins it is
    /// the version itself. See [`project_overlay_index_subject`].
    OverlayIndex {
        /// Which shard.
        shard: OverlayIndexShard,
    },
    /// One membership record, at the row version its publish judged and with
    /// the interval end that version froze.
    ///
    /// **Pinned,**, and the shape of the pin is the one
    /// `pricing_group_membership`'s storage allows: the row carries no
    /// revision-scoped content table, because it is mutated in place, so what a
    /// ref can pin is not "which immutable rows to read" but the one column a
    /// mutation moves. That column is `effective_to`
    /// (`group_membership_repo::end_membership` narrows it; a move is an end
    /// plus a new row), and the row version it belongs to is pinned with it —
    /// which is what makes `effective_to: None` readable as *open-ended* rather
    /// than as *unpinned*. See [`project_membership_subject`].
    GroupMembership {
        /// Which membership.
        membership_id: Uuid,
        /// The row version the publish unit judged. Carried for the refusal
        /// below and for the diagnostics of a projection that finds the row
        /// gone; the content it addresses is the pin's other half.
        row_version: u64,
        /// The interval end that publish judged. `None` is an open-ended
        /// membership — a fact, not an absence.
        effective_to: Option<OffsetDateTime>,
    },
}

/// Read a ref row's subject columns, resolving them into something projectable.
///
/// Every `SubjectKind` is answerable, so this function carries no by-name refusal
/// of an unbuildable kind. A malformed reference still refuses.
///
/// # Errors
/// [`DomainError::Internal`] naming a reference that is not the identifier its
/// kind is keyed by, or a revisioned subject arriving with no revision. There is
/// no other class: every input here comes from this gear's own tables, so a
/// refusal is an invariant breach and never a caller mistake.
fn subject_of(subject: &PendingVersionRow) -> Result<ProjectedSubject, DomainError> {
    match subject.subject_kind {
        SubjectKind::Plan => {
            let plan_id = subject_uuid(subject, "plan").map(PlanId::new)?;
            Ok(ProjectedSubject::Plan {
                plan_id,
                revision: pinned_revision(subject, &plan_id.to_string())?,
                // Likewise the state that revision was in when its publish
                // judged it. The row's own state keeps moving after the commit
                // and before the warm, and `superseded` is a value D-128 does
                // not contemplate for a projected subject at all.
                lifecycle_state: subject.subject_lifecycle_state.ok_or_else(|| {
                    DomainError::Internal(format!(
                        "bss-pricing: catalog version ref {} names plan subject {plan_id} and no \
                         lifecycle state; sellability predicate (4) reads that field at the pin, \
                         and the row's own state has moved on since",
                        subject.pending_ref
                    ))
                })?,
            })
        }
        SubjectKind::PriceOverlay => {
            let price_overlay_id = subject_uuid(subject, "price overlay")?;
            Ok(ProjectedSubject::Overlay {
                price_overlay_id,
                revision: pinned_revision(subject, &price_overlay_id.to_string())?,
            })
        }
        SubjectKind::OverlayIndex => Ok(ProjectedSubject::OverlayIndex {
            shard: OverlayIndexShard::parse(&subject.subject_ref).map_err(|e| {
                DomainError::Internal(format!(
                    "bss-pricing: catalog version ref {} names overlay index subject {}, which \
                     is not a shard key: {e}",
                    subject.pending_ref, subject.subject_ref
                ))
            })?,
        }),
        SubjectKind::GroupMembership => {
            let membership_id = subject_uuid(subject, "group membership")?;
            Ok(ProjectedSubject::GroupMembership {
                membership_id,
                // Refused rather than defaulted, [`pinned_revision`]'s own rule:
                // the row version is what says a pin was written at all, and
                // without it `subject_effective_to`'s `None` is unreadable — an
                // open-ended membership and an unpinned ref spell it the same
                // way. Defaulting either would be a guess about which state a
                // frozen version froze.
                row_version: pinned_revision(subject, &membership_id.to_string())?,
                effective_to: subject.subject_effective_to,
            })
        }
    }
}

/// The subject reference as the `Uuid` its kind is keyed by.
fn subject_uuid(subject: &PendingVersionRow, what: &str) -> Result<Uuid, DomainError> {
    Uuid::parse_str(&subject.subject_ref).map_err(|e| {
        DomainError::Internal(format!(
            "bss-pricing: catalog version ref {} names {what} subject {}, which is not a {what} \
             id: {e}",
            subject.pending_ref, subject.subject_ref
        ))
    })
}

/// The revision the publish judged.
///
/// Refused rather than defaulted to whatever is current. A default here is a
/// guess about which content a frozen version froze, and the guess is wrong
/// exactly when a second publish beat the warm — the case the column was added
/// for.
fn pinned_revision(subject: &PendingVersionRow, id: &str) -> Result<u64, DomainError> {
    subject.subject_revision.ok_or_else(|| {
        DomainError::Internal(format!(
            "bss-pricing: catalog version ref {} names subject {id} and no revision; the \
             revision the publish judged is what a frozen version freezes",
            subject.pending_ref
        ))
    })
}

/// Build one overlay document's delta from the revision its ref pinned.
///
/// **The pinned revision, never the current one**, for the reason
/// [`project_plan_subject`] gives at length one plane over: the projector arrives
/// up to D-47's five-minute batching maximum after the commit, so reading "the
/// overlay's published revision" would freeze content this version's publish
/// never judged — permanently, on an INSERT-only row.
///
/// The revision row is immutable once published, so its content needs no second
/// pinning; that is why this reads the row for everything, including the
/// lifecycle state, where the plan arm reads the ref for its state.
///
/// A pinned revision that is not there is an **internal fault**, not an empty
/// delta — D-128's rule, and an empty overlay document is exactly the shape it
/// exists to prevent.
///
/// # Errors
/// [`DomainError::Internal`] when the pinned revision is absent, or on a storage
/// failure.
async fn project_overlay_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    price_overlay_id: Uuid,
    revision: u64,
) -> Result<OverlaySubjectDelta, DomainError> {
    let record = overlay_repo::load_on(runner, scope, tenant_id, price_overlay_id, revision)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "bss-pricing: price overlay {price_overlay_id} revision {revision} is a subject \
                 of a committed catalog version and is not there; projecting an empty overlay \
                 document is what D-128 exists to prevent"
            ))
        })?;
    Ok(OverlaySubjectDelta {
        content: record.content(),
    })
}

/// Build one membership document's delta — the interval end from the **pin**,
/// the facts no mutation moves from the row.
///
/// # The split, and why it is not the plan arm's split by accident
///
/// [`project_plan_subject`] reads a specific immutable revision for content and
/// the ref for the state that keeps moving. This arm does the same thing with
/// the same reason and a different boundary, because the storage is different:
/// `pricing_group_membership` has no revision-scoped content table — a
/// membership is minted by `enroll` and thereafter mutated **in place** — so
/// "the immutable content" is the row itself, and the moving part is one column.
///
/// `group_membership_repo`'s only in-place mutation is `end_membership`, which
/// narrows `effective_to` and bumps `row_version` (`inst-ms-time`: "ending early
/// = setting `to`"; the atomic move ends one row and **inserts** another, so it
/// moves nothing on an existing row either). `payer_tenant_id`, `group_value`
/// and `effective_from` are written once at enrollment and never again, so
/// reading them from the row reads the same values the publish judged.
/// `effective_to` is the one that can differ by the time the sweep arrives — up
/// to D-47's five-minute batching maximum after the commit — and it comes from
/// the ref row, where the mutation that minted this version froze it
/// (`membership_publish::record_ref`, `pricing_catalog_version_ref`).
///
/// **This is what the premise `MembershipSubjectDelta`'s doc recorded now costs
/// nothing.** While the plane minted one publish unit per membership row the
/// live read was indistinguishable from the pin; Task 6 gave `enroll` and
/// `end_membership` a publish unit each, and the live read then froze the
/// *later* state under the *earlier* commit's version, permanently, on an
/// INSERT-only row a consumer resolves a pin against.
///
/// A membership that is a subject of a committed catalog version and is not
/// there is an **internal fault**, D-128's rule applied here: the row is
/// referenced by a `pricing_catalog_version_ref` row this gear itself wrote, so
/// its absence is an invariant breach and not a caller mistake.
///
/// # Errors
/// [`DomainError::Internal`] when the membership is absent, or on a storage
/// failure.
async fn project_membership_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    membership_id: Uuid,
    row_version: u64,
    effective_to: Option<OffsetDateTime>,
) -> Result<MembershipSubjectDelta, DomainError> {
    let record = group_membership_repo::find(runner, scope, tenant_id, membership_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "bss-pricing: membership {membership_id}, pinned at row version {row_version}, \
                 is a subject of a committed catalog version and is not there; projecting an \
                 empty membership document is what D-128 exists to prevent"
            ))
        })?;
    Ok(MembershipSubjectDelta {
        membership_id: record.membership_id,
        payer_tenant_id: record.payer_tenant_id,
        group_value: record.group_value,
        effective_from: record.effective_from,
        // The pin, never `record.effective_to` — see this function's own doc.
        effective_to,
    })
}

/// Build one plan subject's delta from the truth rows, through the caller's
/// transaction.
///
/// **The source is the revision the publish judged**, pinned on the ref row at
/// commit time and read back here. §4.4 says "the plan's **current** revision",
/// and that is a defect rather than a subtlety: the projector arrives up to the
/// D-47 maximum batching delay — five minutes — after the commit, so a second
/// publish of the same plan inside that window makes its own revision current
/// and "current" then freezes content this version's publish never judged.
/// Permanently, since a delta is INSERT-only on the seven-year horizon in a
/// store whose contract is that a completed version never changes. **Reported.**
///
/// D-128's substance is untouched and is what makes reading a pinned revision
/// safe: the row `(plan_id, revision)` is the same row whether it stands
/// `published` or `retired`, so a retired plan keeps a resolvable delta for the
/// in-flight subscribers D-51 preserves coverage for — and a retirement publish
/// unit, which opens no revision, pins that same number and re-projects it with
/// its new state.
///
/// **The state is pinned too**, and for a sharper reason than the content. The
/// row's own `lifecycle_state` keeps moving after the commit and before the
/// warm — `published -> superseded` at the next revision's commit,
/// `published -> retired` at retirement — and `superseded` is a value D-128
/// does not contemplate for a projected subject and that
/// [`plan_repo::load_current`] could never return. Frozen into an INSERT-only
/// delta it makes the version read **unsellable** to any consumer coding
/// sellability predicate (4) as "is published", which is how D-90 names that
/// predicate's input. The content needs no such pinning — a published revision
/// row and its revision-scoped children are physically immutable — which is why
/// this function reads the **row** for content and the **ref** for state.
///
/// The projector **never reads the open draft revision**: a pinned revision
/// number is a `draft` row's number only if the publish never happened, and the
/// publish is what wrote the ref.
///
/// A pinned revision that does not exist is an **internal fault**, not an empty
/// delta. An empty plan subject is exactly what D-128 was raised to prevent.
///
/// # Errors
/// [`DomainError::Internal`] when the pinned revision is not there, or on a
/// storage failure.
async fn project_plan_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    revision: u64,
    lifecycle_state: LifecycleState,
) -> Result<PlanSubjectDelta, DomainError> {
    let current = plan_repo::load_revision(runner, scope, tenant_id, plan_id, revision)
        .await
        .map_err(|e| repo_failure(&e))?
        .ok_or_else(|| {
            DomainError::Internal(format!(
                "bss-pricing: plan {plan_id} revision {revision} is a subject of a committed \
                 catalog version and is not there; projecting an empty plan subject is what \
                 D-128 exists to prevent"
            ))
        })?;

    // D-121's projected row set, minus its horizon; the module doc carries the
    // premise and names the group that deletes it. Read into a local because the
    // **window** facts are drawn from the same set: they are D-121's one rule over
    // two planes, and a second read taken inside `project_windows` could disagree
    // with this one about which rows the version froze.
    let prices = price_repo::load_for_plan(runner, scope, tenant_id, plan_id, PROJECTED_ROW_STATES)
        .await
        .map_err(|e| repo_failure(&e))?;
    let windows = project_windows(runner, scope, tenant_id, plan_id, &prices).await?;

    // C3's GA gate, derived here — it is a function of the row and a launch
    // constant, so there is nothing to freeze.
    //
    // **D-154's resolved category is NOT derived here.** It is read off
    // `pricing_price.resolved_tax_category`, which the publish commit froze
    // against the readiness the rule set judged the row with. Re-resolving it
    // here was the shape review found (`T-13`): this sweep runs up to D-47's
    // five-minute batching maximum after the commit, and the region taxonomy is
    // a tenant-declared table anyone with `config × write` may re-declare in
    // between — so a version could freeze a category no rule ever judged, or
    // lose one that was present when publish passed.
    let resolved = crate::infra::storage::repo::price_repo::frozen_resolutions(
        runner, scope, tenant_id, plan_id,
    )
    .await
    .map_err(|e| repo_failure(&e))?;
    let tax_projection = prices
        .iter()
        .map(|record| {
            (
                record.price_id,
                RowResolutionProjection {
                    resolved_tax_category: resolved
                        .get(&record.price_id)
                        .and_then(|r| r.tax_category.clone()),
                    // Frozen by the same statement, for the same reason - see
                    // `FrozenResolution`.
                    resolved_rounding_policy: resolved
                        .get(&record.price_id)
                        .and_then(|r| r.rounding_policy.clone()),
                    // `TAX_ENGINE_GA` is a launch constant, not a column: C3
                    // makes the gate a property of the *platform's* Tax Engine
                    // status, and a per-tenant carrier would let one tenant
                    // declare itself post-GA while the engine that computes the
                    // tax does not exist. It flips in code when the engine ships,
                    // and `inst-td-clear` makes every gated plan re-publish to
                    // pick it up rather than flipping silently.
                    not_sellable_ga: is_not_sellable_ga(record, TAX_ENGINE_GA),
                },
            )
        })
        .collect();

    Ok(PlanSubjectDelta {
        plan_id,
        revision,
        lifecycle_state,
        sku_id: current.sku_id,
        plan_tier: current.plan_tier,
        plan_tier_override: current.plan_tier_override,
        billing_cycle: current.billing_cycle,
        frequency: current.frequency,
        available_from: current.available_from,
        available_to: current.available_to,
        purchase_min_qty: current.purchase_min_qty,
        purchase_max_qty: current.purchase_max_qty,
        invoice_grouping_key: current.invoice_grouping_key,
        phases: plan_shape_repo::load_phase_set(runner, scope, tenant_id, plan_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?,
        addon_rules: plan_shape_repo::load_addon_rule_set(
            runner, scope, tenant_id, plan_id, revision,
        )
        .await
        .map_err(|e| repo_failure(&e))?,
        descriptor_set: plan_shape_repo::load_descriptor(
            runner, scope, tenant_id, plan_id, revision,
        )
        .await
        .map_err(|e| repo_failure(&e))?,
        // D-319's period floor/cap, from the same revision-scoped table the
        // publish rules judged and the content pin framed. Without this line
        // the delta renders an empty list on every plan and PRD §17.8's field
        // would be authorable, approvable and unreachable by Rating - the shape
        // the line below records `composites` having been in.
        period_floor_caps: plan_shape_repo::load_period_floor_cap_set(
            runner, scope, tenant_id, plan_id, revision,
        )
        .await
        .map_err(|e| repo_failure(&e))?,
        // Slice 10's derived meters, from the same revision-scoped table the
        // publish rules judged and the content pin framed. **Without this line
        // the delta renders an empty list on every plan** and `inst-cm-frozen`
        // would hold on the approval side and nowhere a consumer can read - the
        // same shape D-257 found one layer down, where the publish assembly's
        // missing `shape.composites` left two rules iterating an empty vector.
        composites: plan_shape_repo::load_composite_set(
            runner, scope, tenant_id, plan_id, revision,
        )
        .await
        .map_err(|e| repo_failure(&e))?,
        entitlement_grants: current.entitlement_grants,
        change_contract: current.change_contract,
        prices,
        tax_projection,
        windows,
    })
}

/// Build one `overlay_index` shard from the overlays live **at this version**.
///
/// # Derived, not pinned, and the derivation is the decision
///
/// S9 §7 requires the index to enumerate *"every scope-matching overlay **live at
/// V**"*, and pairs it with one per-subject document read per matching overlay.
/// Read from the overlay table — "what is published now" — a second overlay
/// publishing inside D-47's five-minute batching window would land in the
/// *earlier* version's shard, and a consumer pinned at `V` would enumerate an
/// overlay whose document has no delta at or below `V`. The document read the
/// same sentence prescribes then finds nothing. That is §4.4's "current revision
/// is the wrong source" defect a third time, and permanent, on an INSERT-only
/// row.
///
/// So the set comes from `pricing_catalog_version_ref`, at each overlay's
/// **greatest** ref at or below `V` — Foundation §4.4's greatest-completed-≤-pin
/// rule, applied to choose which revision the *index* speaks of.
///
/// **The version's own refs are counted even though they are not finalized yet**,
/// and this is the clause the whole thing turns on. The two subjects of one
/// overlay publish are projected in two transactions, so a shard derived from
/// committed refs alone would omit **its own sibling** whenever the shard was
/// projected first — and ordering the subjects would not fix it, because a
/// document whose projection failed would leave the shard warm, wrong and never
/// re-derived. `siblings` is the same union [`version_is_complete`] makes, for
/// the same reason.
///
/// **This is also what gives D-133's two-shard case with no special case.** A
/// revision that moves its scope value appears under whichever shard its
/// greatest-≤-`V` revision names, so it leaves the old shard and joins the new
/// one at the version that moved it — and both shards are subjects of that
/// publish, so both are re-derived.
///
/// **D-133's horizon is absent, and it is the same absence D-121's is.** The
/// module doc's premise 1 carries the argument in full and this shares it rather
/// than stating a second one: `H` has no producer in this tree, and the set it
/// would bound is the tenant's live overlays rather than an accumulation, because
/// an overlay revision leaves the index only by being superseded by its own
/// successor — which replaces it in the set rather than adding to it.
///
/// # What this costs, and it is **not** bounded by the shard (open)
///
/// The horizon paragraph above is about the shard's *contents*, which are indeed
/// the tenant's live overlays rather than an accumulation. The **reads** that
/// produce them are neither shard-bounded nor live-bounded, and review
/// RUST-PERF-001 measured the shape rather than the number:
///
/// * `overlay_revisions_at_or_below` scans every `PriceOverlay` ref row the
///   tenant has ever committed at or below `V` — an accumulation that grows with
///   every overlay publish for the life of the catalog — and folds it to the
///   live set;
/// * the loop then issues one `overlay_repo::load_on` per live overlay of the
///   **whole tenant**, and discards the ones whose revision does not file under
///   this shard only after reading them.
///
/// So projecting one shard costs O(the tenant's live overlays) queries inside the
/// projection transaction plus a full historical ref scan, and the sharding D-112
/// introduced to bound overlay enumeration buys nothing on the write path.
///
/// **Closing it is a repository change, not one here.** The shard is a property
/// of the *revision's* stored scope, which the ref row does not carry, so the
/// restriction has to be either a join onto `pricing_price_overlay` at
/// `(price_overlay_id, subject_revision)` inside `overlay_revisions_at_or_below`
/// or a shard key carried on the ref row itself; and the per-overlay document
/// read wants a batched `load_on` over the surviving `(id, revision)` pairs.
/// Both live in `infra::storage::repo`. Recorded here rather than left to be
/// rediscovered, because this is the function whose cost it is.
///
/// # Errors
/// [`DomainError::Internal`] when a listed overlay's pinned revision is not
/// there, when a stored scope cannot be read as a shard key, or on a storage
/// failure.
async fn project_overlay_index_subject(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
    siblings: &[PendingVersionRow],
    shard: OverlayIndexShard,
) -> Result<OverlayIndexDelta, DomainError> {
    let mut live = catalog_version_ref_repo::overlay_revisions_at_or_below(
        runner,
        scope,
        tenant_id,
        catalog_version,
    )
    .await
    .map_err(|e| repo_failure(&e))?;

    // The refs of *this* version, which carry no version in storage until their
    // own transaction finalizes them. They are at `V` by definition - that is
    // what the pass resolved them to - so they win over anything older.
    for sibling in siblings {
        if sibling.subject_kind != SubjectKind::PriceOverlay {
            continue;
        }
        let (Ok(price_overlay_id), Some(revision)) = (
            Uuid::parse_str(&sibling.subject_ref),
            sibling.subject_revision,
        ) else {
            return Err(DomainError::Internal(format!(
                "bss-pricing: catalog version ref {} names a price overlay subject this \
                 projector cannot read: {} at revision {:?}",
                sibling.pending_ref, sibling.subject_ref, sibling.subject_revision
            )));
        };
        live.insert(price_overlay_id, revision);
    }

    let mut entries = Vec::new();
    for (price_overlay_id, revision) in live {
        let record = overlay_repo::load_on(runner, scope, tenant_id, price_overlay_id, revision)
            .await
            .map_err(|e| repo_failure(&e))?
            .ok_or_else(|| {
                DomainError::Internal(format!(
                    "bss-pricing: price overlay {price_overlay_id} revision {revision} is named \
                     by a catalog version ref and is not there; the index would enumerate an \
                     overlay no document read can resolve"
                ))
            })?;
        // Which shard this revision belongs to is a property of the revision,
        // not of the overlay: a revision that moved the scope value files the
        // overlay elsewhere from this version on.
        if OverlayIndexShard::try_from(&record.scope)? != shard {
            continue;
        }
        entries.push(OverlayIndexEntry {
            price_overlay_id,
            interval: record.interval,
            precedence: record.precedence,
        });
    }
    Ok(OverlayIndexDelta { shard, entries })
}

/// The plan's window facts as the delta freezes them (D-99, D-121).
///
/// Four steps and each is somebody else's statement rather than this function's:
/// the store's plan-wide read, D-121's **row** set (via `projected`, already
/// filtered by [`PROJECTED_ROW_STATES`]), [`PROJECTED_WINDOW_STATES`]'s window
/// filter, and [`window::group_by_key_seeded`]'s grouping and ordering.
///
/// **The filters are why this is not just the repository read.**
/// `window_repo::list_for_plan` deliberately returns cancelled windows and the
/// windows of **every** price row of the plan, drafts included — the operator's
/// coverage report has to be able to say a key lost its successor to a
/// cancellation, and `inst-wc-required` makes a publish fail without coverage, so
/// a window must be schedulable on a draft row. D-121 keeps both out of the read
/// model just as deliberately: it projects "every price row of the plan — any
/// lifecycle state **except never-published drafts** — whose window set intersects
/// the horizon". Two different questions, one store read, and the difference is
/// exactly the two filters here.
///
/// **The row filter is by `price_id`, never by `scope_key`**, and that is
/// load-bearing rather than incidental. A draft row and a published row
/// legitimately share one canonical scope key — Foundation §3.7's two partial
/// `UNIQUE` predicates are disjoint, and a draft successor coexists with the row
/// it will supersede — so a key-based filter would either drop the published row's
/// own windows or keep the draft's. Before the filter existed, a draft's window on
/// a published row's key moved the frozen `coverageEnd` forward, and the pinned
/// read model then advertised coverage no published row backed: sellability
/// predicate (1) reads such a key as sellable into an interval nothing published is
/// effective over, which is the D-99 exposure arriving from the projection side.
///
/// **The group set is seeded from the projected keys**, so `windows` enumerates
/// exactly the keys `prices` does. That answers two things at once: a key whose
/// only window was cancelled renders `Uncovered` instead of vanishing — the
/// declared spelling of a payload fact rather than the absence of a group — and a
/// draft row on a key of its own contributes no group, where before it produced one
/// for a key no projected row backs.
///
/// # Errors
/// [`DomainError::Internal`] on a storage failure, or on a window whose price row
/// is not on the plan — which the foreign key makes an invariant breach.
async fn project_windows(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    plan_id: PlanId,
    projected: &[PriceRecord],
) -> Result<Vec<KeyWindows>, DomainError> {
    let records = window_repo::list_for_plan(runner, scope, tenant_id, plan_id)
        .await
        .map_err(|e| repo_failure(&e))?;
    Ok(window::group_by_key_seeded(
        projected.iter().map(|row| row.scope_key.clone()),
        records
            .into_iter()
            .filter(|w| {
                PROJECTED_WINDOW_STATES.contains(&w.state)
                    && projected.iter().any(|row| row.price_id == w.price_id)
            })
            .map(|w| {
                (
                    w.scope_key,
                    WindowInterval::new(w.effective_from, w.effective_to, w.state),
                )
            }),
    ))
}

/// Is every subject of `catalog_version` warm?
///
/// The subject set is the version's **committed** refs **union what the caller
/// already knows** — the refs this pass has resolved to this version and is in
/// the middle of finalizing. Both halves are load-bearing, and the second is
/// the one an earlier version of this function did without: a ref the pass has
/// resolved but not yet finalized carries no version in storage, so
/// [`catalog_version_ref_repo::list_at_version`] cannot see it, and a version
/// with two subjects read as complete after the first was warmed. Advancing
/// there is the D-101 divergence with the frontier asserting the opposite.
///
/// A version with no refs at all answers `false`. It cannot arise — a version
/// exists in this store because a ref points at it — and answering `true` would
/// let the frontier advance over a version whose subject set nobody knows.
async fn version_is_complete(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
    known: &[PendingVersionRow],
) -> Result<bool, DomainError> {
    let subjects = version_subjects(runner, scope, tenant_id, catalog_version, known).await?;
    if subjects.is_empty() {
        return Ok(false);
    }
    let warm = read_model_repo::warm_subjects_at(runner, scope, tenant_id, catalog_version)
        .await
        .map_err(|e| repo_failure(&e))?;
    Ok(outstanding_subjects(&subjects, &warm).is_empty())
}

/// Refuse to project into a version the frontier has already declared
/// pin-eligible.
///
/// **This is the enforcement the premise above cannot provide.** A finalize
/// targeting a version at or below the frontier is an already-pin-eligible
/// version acquiring a **new subject** — which is exactly one pin resolving two
/// different contents over time, the divergence D-101 and D-114 exist to close.
/// It is detected at the moment it would corrupt rather than predicted, so a
/// registry that violates batch atomicity produces a loud, operator-visible
/// fault instead of a silently wrong frozen version.
///
/// It cannot fire on any correct path: the frontier standing at `V` asserts
/// every version at or below `V` is complete, so no subject of such a version
/// can still be outstanding. If one is, the frontier already lied.
///
/// # What it costs when it fires, and what keeps that cost attached to a
/// contract violation
///
/// The price is total for that publish and D-163 clause (3) states it beside the
/// guarantee: the publish becomes **unresolvable at any version**, its subject is
/// never projected, its ref never finalizes, both Critical alarms fire every
/// pass, and **nothing self-heals it** — the remedy is out of band, §3.6's own
/// "a registry re-request, never a silent re-emit".
///
/// A price like that may only be charged for a **broken contract**, and as first
/// built this guard could reach it through an ordinary **per-ref registry
/// outage**: a version whose sibling ref the registry could not answer read
/// complete from the warm set alone, the frontier advanced, and the sibling
/// arrived below it a tick later. [`PassCoverage`] is what removed that path —
/// a pass that could not have seen the whole subject set decides no completion,
/// so the frontier never passes a version whose set the pass was still guessing
/// at. What is left reaching this refusal is the registry contradicting clause
/// (1): a batch that did not commit atomically into one version. That is the
/// case it was written for, and it is the case it should be loud about.
///
/// # Errors
/// [`DomainError::Internal`] naming the version, the frontier and the handle.
async fn refuse_projection_below_frontier(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    catalog_version: CatalogVersion,
    pending_ref: &str,
) -> Result<(), DomainError> {
    let Some(frontier) = pin_frontier_repo::read_at(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?
    else {
        return Ok(());
    };
    if catalog_version > frontier.catalog_version {
        return Ok(());
    }
    Err(DomainError::Internal(format!(
        "bss-pricing: pending ref {pending_ref} of tenant {tenant_id} resolves to catalog \
         version {}, at or below the pin-eligibility frontier standing at {}; that version is \
         already pinnable and acquiring a subject now would make one pin resolve two contents",
        catalog_version.get(),
        frontier.catalog_version.get()
    )))
}

/// Advance the frontier to `completed`, then walk it forward over every
/// following version that is already complete.
///
/// The first half is D-136 as written: the advance happens only when
/// `completed` is the frontier's **next** version in order, which is what makes
/// the D-114 prefix true by construction rather than re-derived. When it is
/// not, nothing moves and that is the prefix holding — an earlier version is
/// still outstanding.
///
/// # What makes "complete" trustworthy, and what happens when it is not
///
/// Completeness is judged from the version's **committed** refs plus the ones
/// this pass is finalizing, so it is only as good as the premise that a
/// **batch commits atomically into one version** — which is what "the registry
/// batches approved publishes and emits `CatalogVersionPublished`" (§4.2 step 5,
/// §3.6) means on its face: one batch, one version, one event. Under it, any
/// committed ref of `V` implies `V`'s subject set is **closed**, and the only
/// open question is warmth.
///
/// The premise is **not stated by any document in the set**, and it is not
/// predicted here either. What guards it is
/// [`refuse_projection_below_frontier`], which detects the violation **at the
/// moment it would corrupt**: a ref resolving into a version the frontier has
/// already passed is refused loudly rather than quietly added to a pinnable
/// version. **Reported.**
///
async fn advance_frontier(
    runner: &impl DBRunner,
    scope: &AccessScope,
    tenant_id: Uuid,
    completed: CatalogVersion,
    now: OffsetDateTime,
) -> Result<Option<CatalogVersion>, DomainError> {
    let standing = pin_frontier_repo::read_at(runner, scope, tenant_id)
        .await
        .map_err(|e| repo_failure(&e))?
        .map(|frontier| frontier.catalog_version);
    let next =
        catalog_version_ref_repo::next_committed_version_after(runner, scope, tenant_id, standing)
            .await
            .map_err(|e| repo_failure(&e))?;
    if next != Some(completed) {
        return Ok(None);
    }

    pin_frontier_repo::advance(runner, scope, tenant_id, completed, now)
        .await
        .map_err(|e| repo_failure(&e))?;
    let mut reached = completed;

    // The walk. Every step is still "the frontier's next version in order" and
    // still complete, so it is strictly forward and never past a gap.
    loop {
        let following = catalog_version_ref_repo::next_committed_version_after(
            runner,
            scope,
            tenant_id,
            Some(reached),
        )
        .await
        .map_err(|e| repo_failure(&e))?;
        let Some(following) = following else {
            break;
        };
        // `&[]`: this pass knows nothing extra about a version it is not
        // projecting, so the walk sees only committed refs. Under batch
        // atomicity that is the whole subject set; when it is not,
        // `refuse_projection_below_frontier` catches the straggler loudly
        // instead of letting it join a pinnable version.
        if !version_is_complete(runner, scope, tenant_id, following, &[]).await? {
            break;
        }
        pin_frontier_repo::advance(runner, scope, tenant_id, following, now)
            .await
            .map_err(|e| repo_failure(&e))?;
        reached = following;
    }
    Ok(Some(reached))
}

#[cfg(test)]
#[path = "read_model_tests.rs"]
mod read_model_tests;
