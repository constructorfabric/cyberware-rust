//! Background jobs the gear's `stateful` `serve` loop drives.
//!
//! **They are independent**, and the module list below is the roster: each job has
//! its own coordination lease, its own cadence and its own pass, and none reads what
//! another writes. That independence is worth stating because it is what makes them
//! separate tickers rather than phases of one — a window flipping does not wait on a
//! warm, a warm does not wait on a window, and neither waits on a gauge refresh.
//!
//! **No count.** This paragraph opened *"Two, and they are independent"* and the
//! bullets named two of three for the two days after [`gated_markets`] landed
//! (F-6/Z10-8). A count in prose beside a roster in code leaves only one of the two
//! true, and it is never the prose — which is the correction the first bullet below
//! already carries for its own earlier count.
//!
//! - [`readmodel_warm`] — §3.8's read-model warm re-drive: resolve pending
//!   `CatalogVersion` handles against the registry, drive
//!   [`ReadModelProjector`](crate::infra::read_model::ReadModelProjector) over
//!   each version, raise the two Critical alarms §3.6 and §4.4 name by string,
//!   and enqueue `PlanPublishDegraded` for a publish whose subject is still
//!   not warm. **It is the only thing that turns a publish into something a
//!   consumer can pin**: without its ticker nothing ever resolves the pending
//!   handle the commit left behind, and `pricing_read_model` stays empty
//!   whatever else is built. (No count of the jobs is written here: a count in
//!   prose beside a roster in code goes stale on the next one, and the claim worth
//!   making is about this job's necessity rather than about how many there are.)
//! - [`window_activation`] — `07-pricewindow-linkage.md` §4's two time-driven
//!   transitions: flip `scheduled → active` at `effectiveFrom` and
//!   `active → expired` at `effectiveTo`, emitting `PriceWindowActivated` /
//!   `PriceWindowExpired` from the outbox, ordered per `(tenant, plan)`, and
//!   raise the Warn alarm §7 names for a boundary that has stood uncrossed past
//!   the job SLO. It is **not** a publish unit and re-projects nothing
//!   (`inst-ws-publishunit`), which is what the read model carrying window
//!   *intervals* buys.
//! - [`gated_markets`] — D-246's catalog-wide GA backlog on D-250's cadence: count
//!   the tenant-markets a published tax-inclusive row gates while `TAX_ENGINE_GA`
//!   stands false, and publish it to `pricing_tax_not_sellable_ga`. **The one job
//!   here that is not load-bearing for correctness** — the gear serves every request
//!   without it — and what its absence costs is §7's alarm never firing, because the
//!   gauge is an observable over a cached value and an unrefreshed cache reports `0`
//!   while markets are gated.
//!
//! These are **system-context, cross-tenant** jobs, exactly as the sibling
//! ledger's are: they read across tenants under the sanctioned
//! [`AccessScope::allow_all`](toolkit_db::secure::AccessScope::allow_all) system
//! scope with the actor
//! [`SecurityContext::anonymous`](toolkit_security::SecurityContext::anonymous),
//! and **narrow to `AccessScope::for_tenant` before any per-tenant write**.

pub mod gated_markets;
pub mod readmodel_warm;
pub mod window_activation;

/// Say that an alarm's age threshold could not be **converted**, so the alarm
/// was not evaluated this pass.
///
/// Every alarm in these jobs is "this thing is older than `<knob>`", and every
/// one of them reaches its comparison through
/// `time::Duration::try_from(self.jobs.<knob>())`. Four of those conversions
/// were `let Ok(..) else { return; }` with no log, no counter and no report
/// member: a deployment whose knob will not convert simply stopped raising
/// that Critical or Warn, and the pass looked exactly like a pass with nothing
/// to raise.
///
/// **Operationally unreachable, and logged anyway.** `from_std` fails past
/// roughly 292 million years and [`JobsConfig::validate`](crate::config::JobsConfig)
/// refuses zero and orders tick < overdue — but it sets no upper bound, so the
/// value is a deployment's to make unconvertible. What earns the four lines is
/// this plane's own house rule, stated at `readmodel_warm`'s `frontier_ages`: a
/// guard that could not be **evaluated** must say so rather than look like a
/// guard that passed.
///
/// **`degraded_guard`, never `alarm`.** `alarm = …` is what an operator's
/// runbook greps for and what `metrics.alarm` labels; stamping it here would
/// report the alarm as *raised*, where what happened is that it could not be
/// judged at all. `frontier_ages` names the same distinction and this is the
/// same field.
pub(crate) fn unconvertible_threshold(
    error: time::error::ConversionRange,
    knob: &'static str,
    guarded: &str,
) {
    tracing::error!(
        error = ?error,
        knob,
        degraded_guard = guarded,
        "bss-pricing: a job's age threshold could not be converted to a chrono duration, so the \
         guard it bounds was not evaluated this pass and the alarm it raises cannot fire; the \
         configured value is out of range rather than merely large"
    );
}
