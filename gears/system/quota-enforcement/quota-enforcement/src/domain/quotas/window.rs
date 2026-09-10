//! Validity-window semantics (`features/quota-lifecycle.md`, "Validity Window
//! Semantics").
//!
//! The window is stored verbatim and never acts on the lifecycle: the core does
//! not deactivate a Quota whose end has passed, and the Engine decides what an
//! out-of-window Quota means at evaluation time. Reads compute one boolean,
//! `currently_within_window`, from one clock reading per response.

use std::collections::HashMap;
use std::hash::BuildHasher;

use quota_enforcement_sdk::{MetricId, MetricKind, PageResult, Quota, QuotaView};
use time::OffsetDateTime;

/// The public view of one stored Quota at `now`.
// @cpt-algo:cpt-cf-quota-enforcement-algo-validity-window:p1
#[must_use]
pub fn view(quota: Quota, metric_kind: Option<MetricKind>, now: OffsetDateTime) -> QuotaView {
    // @cpt-begin:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-store
    // The window is a structural field of the stored record; the draft carried
    // it verbatim into storage and nothing rewrites it here.
    // @cpt-end:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-store
    // @cpt-begin:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-no-auto
    // The lifecycle status is read as stored: `now > end` changes the boolean
    // below, never the status.
    // @cpt-end:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-no-auto
    // @cpt-begin:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-compute
    // `[start, end]` inclusive, an absent bound unbounded on its side.
    let view = QuotaView::compute(quota, metric_kind, now);
    // @cpt-end:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-compute
    // @cpt-begin:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-return
    view
    // @cpt-end:cpt-cf-quota-enforcement-algo-validity-window:p1:inst-qvw-return
}

/// The public view of a page, every item at the same `now`. A metric absent
/// from `kinds` yields `metric_kind: None`.
#[must_use]
pub fn view_page<S: BuildHasher>(
    page: PageResult<Quota>,
    kinds: &HashMap<MetricId, MetricKind, S>,
    now: OffsetDateTime,
) -> PageResult<QuotaView> {
    page.map(|quota| {
        let kind = kinds.get(&quota.metric).copied();
        view(quota, kind, now)
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "window_tests.rs"]
mod window_tests;
