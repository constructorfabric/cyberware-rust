//! [`resolve_active_membership`] at every boundary — no database, on purpose:
//! the rule is arithmetic over a handful of instants, cheaper and clearer to
//! state that way than to seed as rows. `tests/sqlite_group_membership_repo.rs`
//! is where `enroll`/`end_membership`/`intervals_for_payer` are proved through
//! the repository, against a store that has to enforce D-09; this file is
//! where the narrowing rule they feed is proved on its own.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{MembershipRow, resolve_active_membership};
use crate::domain::instant::utc_ymd_hms;
use time::OffsetDateTime;

fn t(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 10, hour, 0, 0)
}

/// One membership row over `[from, to)`, distinguishable from a sibling by
/// `group_value` so a test can tell which of several candidates resolution
/// picked.
fn membership(
    group_value: &str,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> MembershipRow {
    MembershipRow {
        membership_id: Uuid::new_v4(),
        tenant_id: Uuid::from_u128(0x7e_11),
        payer_tenant_id: Uuid::from_u128(0xba_11),
        group_value: group_value.to_owned(),
        effective_from: from,
        effective_to: to,
        created_by: Uuid::from_u128(0xac_10),
        created_at: t(0),
        row_version: 0,
    }
}

#[test]
fn an_instant_strictly_inside_the_interval_resolves() {
    let rows = vec![membership("gold", t(1), Some(t(5)))];
    let resolved = resolve_active_membership(&rows, t(3)).expect("t(3) sits inside [t1, t5)");
    assert_eq!(resolved.group_value, "gold");
}

#[test]
fn effective_from_is_included() {
    // The half-open rule's inclusive end: `covers` at `t == effective_from`.
    let rows = vec![membership("gold", t(1), Some(t(5)))];
    let resolved = resolve_active_membership(&rows, t(1)).expect("effective_from is included");
    assert_eq!(resolved.group_value, "gold");
}

#[test]
fn effective_to_is_excluded() {
    // The half-open rule's other half. A single interval with no successor, so
    // this is the case `window::WindowInterval::covers` names as `at < end`
    // failing rather than a gap-detection question — nothing here claims the
    // instant at all.
    let rows = vec![membership("gold", t(1), Some(t(5)))];
    assert!(
        resolve_active_membership(&rows, t(5)).is_none(),
        "effective_to is excluded: the interval does not cover its own end"
    );
}

#[test]
fn an_instant_outside_every_interval_resolves_to_none() {
    let rows = vec![membership("gold", t(1), Some(t(5)))];
    assert!(resolve_active_membership(&rows, t(9)).is_none());
    // Before the first interval starts, too — not just after the last ends.
    assert!(resolve_active_membership(&rows, t(0)).is_none());
}

#[test]
fn two_sequential_intervals_each_resolve_to_their_own() {
    // Adjacent at t(5): the first ends where the second begins, and the
    // boundary instant belongs to the later one and to nothing else -
    // `window_repo::intersects`' own half-open reading, over this table's
    // rows.
    let rows = vec![
        membership("gold", t(1), Some(t(5))),
        membership("platinum", t(5), Some(t(9))),
    ];

    assert_eq!(
        resolve_active_membership(&rows, t(3))
            .expect("inside the first")
            .group_value,
        "gold"
    );
    assert_eq!(
        resolve_active_membership(&rows, t(5))
            .expect("the boundary instant belongs to the second interval")
            .group_value,
        "platinum"
    );
    assert_eq!(
        resolve_active_membership(&rows, t(7))
            .expect("inside the second")
            .group_value,
        "platinum"
    );
    assert!(
        resolve_active_membership(&rows, t(9)).is_none(),
        "the second interval's own effective_to is excluded exactly as the first's was"
    );
}

#[test]
fn an_open_ended_interval_resolves_arbitrarily_far_into_the_future() {
    let rows = vec![membership("gold", t(1), None)];
    assert_eq!(
        resolve_active_membership(&rows, t(1))
            .expect("effective_from is still included")
            .group_value,
        "gold"
    );
    let far_future = utc_ymd_hms(2099, 1, 1, 0, 0, 0);
    assert_eq!(
        resolve_active_membership(&rows, far_future)
            .expect("effective_to = None means open-ended, not unbounded-in-name-only")
            .group_value,
        "gold"
    );
    assert!(
        resolve_active_membership(&rows, t(0)).is_none(),
        "open-ended is not the same as unbounded on the left"
    );
}
