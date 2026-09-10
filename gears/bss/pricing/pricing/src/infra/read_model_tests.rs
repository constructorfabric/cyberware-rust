//! The two pieces of the projector that are decidable without a database.
//!
//! Everything else here is a statement about rows in several tables at once —
//! the finalize, the delta, the completeness check, the frontier walk — and is
//! proved in `tests/sqlite_read_model.rs` against a real one. What is worth
//! isolating is the subtraction that **is** the re-drive, and the refusal of a
//! subject kind this gear cannot have written: the first because a wrong
//! subtraction re-projects a warm subject (refused by the primary key, but as a
//! failed pass rather than as a no-op), and the second because its whole value
//! is the sentence it produces.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use uuid::Uuid;

use super::{ProjectedSubject, outstanding_subjects, subject_of};
use crate::domain::error::DomainError;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::read_model::{OverlayIndexShard, SubjectKind, SubjectRef};
use crate::infra::storage::repo::PendingVersionRow;

fn ref_row(handle: &str, subject: &SubjectRef) -> PendingVersionRow {
    PendingVersionRow::for_subject(
        Uuid::from_u128(0x7e_11),
        handle.to_owned(),
        subject,
        Some(0),
        Some(LifecycleState::Published),
        utc_ymd_hms(2026, 8, 3, 12, 0, 0),
    )
}

#[test]
fn the_re_drive_is_the_subtraction_of_the_warm_set() {
    let a = Uuid::from_u128(0xa);
    let b = Uuid::from_u128(0xb);
    let subjects = vec![
        ref_row("pend-a", &SubjectRef::Plan(a)),
        ref_row("pend-b", &SubjectRef::Plan(b)),
    ];

    // First warm: the difference is everything.
    assert_eq!(outstanding_subjects(&subjects, &[]).len(), 2);

    // A re-drive after one of the two landed: the difference is exactly what
    // failed last time. Not a second mechanism - this subtraction is the
    // whole of sec 4.4's unbounded degraded re-drive.
    let warm = vec![(SubjectKind::Plan, a.to_string())];
    let outstanding = outstanding_subjects(&subjects, &warm);
    assert_eq!(outstanding.len(), 1);
    assert_eq!(outstanding[0].subject_ref, b.to_string());

    // Complete: nothing outstanding, which is what lets the frontier advance.
    let warm = vec![
        (SubjectKind::Plan, a.to_string()),
        (SubjectKind::Plan, b.to_string()),
    ];
    assert!(outstanding_subjects(&subjects, &warm).is_empty());
}

#[test]
fn the_warm_set_is_matched_on_the_kind_as_well_as_the_reference() {
    // Two subject kinds can carry the same uuid - a plan and a membership
    // record are unrelated rows keyed alike - so a subtraction that compared
    // references alone would report a plan warm because a membership subject
    // of the same id was.
    let id = Uuid::from_u128(0xa);
    let subjects = vec![ref_row("pend-a", &SubjectRef::Plan(id))];
    let warm = vec![(SubjectKind::GroupMembership, id.to_string())];

    assert_eq!(
        outstanding_subjects(&subjects, &warm).len(),
        1,
        "a membership subject warm at this version says nothing about the plan"
    );
}

// There used to be a test here asserting that some `SubjectKind` this
// projector cannot build is refused by name. **`PriceOverlay` left that case
// when the document arm landed, `OverlayIndex` when the shard arm did, and
// `GroupMembership` leaves it with this task (task 5): every member of
// `SubjectKind::ALL` now resolves to a `ProjectedSubject`.** The case is
// deleted rather than kept pointed at nothing, because a `SubjectKind::ALL`
// with no member left to name would make the test either not compile (E0004,
// were it a `match`) or silently vacuous - and D-91's per-subject-kind
// exhaustiveness is exactly the property `a_plan_subject_whose_reference_is_not_a_plan_id_is_refused`
// and its two membership siblings below still hold per kind, over the failure
// mode each kind's own reference parsing can still produce - one case per kind,
// below.

#[test]
fn an_overlay_subject_whose_reference_is_not_a_uuid_is_refused() {
    // The two arms that had no refusal case of their own. A rewrite folding
    // either into a default rather than `DomainError::Internal` would leave the
    // crate green while the projector accepted a subject no writer produces —
    // and the `OverlayIndex` arm is the likeliest to drift, being the one whose
    // reference is a shard key rather than a bare uuid.
    let mut row = ref_row("pend-o", &SubjectRef::PriceOverlay(Uuid::from_u128(2)));
    row.subject_ref = "not-a-uuid".to_owned();

    let refusal = subject_of(&row).expect_err("an overlay subject is keyed by an overlay id");
    assert!(matches!(refusal, DomainError::Internal(_)), "{refusal:?}");
}

#[test]
fn an_overlay_index_subject_whose_reference_is_not_a_shard_key_is_refused() {
    let mut row = ref_row(
        "pend-i",
        &SubjectRef::OverlayIndex(OverlayIndexShard::parse("global/global").expect("a shard key")),
    );
    row.subject_ref = "not-a-shard-key".to_owned();

    let refusal = subject_of(&row).expect_err("an overlay index subject is keyed by a shard");
    assert!(matches!(refusal, DomainError::Internal(_)), "{refusal:?}");
}

#[test]
fn a_membership_subject_whose_reference_is_not_a_uuid_is_refused() {
    // `for_subject` writes the kind and the reference together, so this is
    // unreachable through that constructor and reachable only through a row
    // something else wrote - `a_plan_subject_whose_reference_is_not_a_plan_id_is_refused`'s
    // reason, one kind over.
    let mut row = ref_row("pend-x", &SubjectRef::GroupMembership(Uuid::from_u128(1)));
    row.subject_ref = "not-a-uuid".to_owned();

    let refusal = subject_of(&row).expect_err("a membership subject is keyed by a membership id");
    assert!(matches!(refusal, DomainError::Internal(_)), "{refusal:?}");
}

#[test]
fn a_membership_subject_resolves_to_the_state_its_publish_judged() {
    // No lifecycle state to pin, unlike the plan arm - a membership has none -
    // but a **row version** and an **interval end**, which is what
    // `PendingVersionRow::for_membership` writes and what the projector reads
    // instead of the row's own `effective_to`. An open-ended pin is the case
    // below; the interleaving that makes the distinction observable is
    // `sqlite_read_model`'s
    // `each_of_two_publish_units_over_one_membership_freezes_the_state_it_judged`.
    let id = Uuid::from_u128(0x_9e_bb);
    let row = PendingVersionRow::for_membership(
        Uuid::from_u128(0x7e_11),
        "pend-membership".to_owned(),
        id,
        0,
        None,
        utc_ymd_hms(2026, 8, 12, 12, 0, 0),
    );
    let resolved =
        subject_of(&row).expect("a membership subject names the membership it publishes");
    let ProjectedSubject::GroupMembership {
        membership_id,
        row_version,
        effective_to,
    } = resolved
    else {
        panic!("a membership ref resolves to a membership subject");
    };
    assert_eq!(membership_id, id);
    assert_eq!(row_version, 0);
    assert_eq!(
        effective_to, None,
        "an open-ended membership pins `None`, which is a fact and not an absent pin"
    );
}

#[test]
fn a_membership_subject_with_no_pinned_row_version_is_refused_rather_than_read_live() {
    // The refusal is what makes `subject_effective_to: None` mean *open-ended*.
    // Both an unpinned ref and a live membership spell that column `NULL`, so
    // without a second column saying a pin was written, the projector would have
    // to guess - and the guess is wrong exactly when a second mutation beat the
    // warm, which is the case the pin exists for. The row version is that second
    // column, `a_plan_subject_with_no_pinned_revision_is_refused_rather_than_defaulted`'s
    // rule one kind over.
    let id = Uuid::from_u128(0x_9e_bb);
    let mut row = PendingVersionRow::for_membership(
        Uuid::from_u128(0x7e_11),
        "pend-membership".to_owned(),
        id,
        3,
        None,
        utc_ymd_hms(2026, 8, 12, 12, 0, 0),
    );
    row.subject_revision = None;

    let refusal = subject_of(&row).expect_err("a membership subject pins the version it judged");
    match refusal {
        DomainError::Internal(message) => assert!(
            message.contains("no revision"),
            "the refusal must say what is missing, got: {message}"
        ),
        other => panic!("expected an internal fault, got {other:?}"),
    }
}

#[test]
fn a_plan_subject_whose_reference_is_not_a_plan_id_is_refused() {
    // The kind and the reference are written together by `for_subject`, so this
    // is unreachable through that constructor - and reachable through a row
    // something else wrote, which is the case the parse exists for.
    let mut row = ref_row("pend-x", &SubjectRef::Plan(Uuid::from_u128(1)));
    row.subject_ref = "not-a-uuid".to_owned();

    let refusal = subject_of(&row).expect_err("a plan subject is keyed by a plan id");
    assert!(matches!(refusal, DomainError::Internal(_)), "{refusal:?}");
}

#[test]
fn a_plan_subject_resolves_to_its_plan() {
    let id = Uuid::from_u128(0x9_1a4);
    let resolved = subject_of(&ref_row("pend-a", &SubjectRef::Plan(id)))
        .expect("a plan subject names a plan, the revision its publish judged, and its state");
    let ProjectedSubject::Plan {
        plan_id,
        revision,
        lifecycle_state,
    } = resolved
    else {
        panic!("a plan ref resolves to a plan subject");
    };
    assert_eq!(plan_id.get(), id);
    assert_eq!(revision, 0);
    assert_eq!(lifecycle_state, LifecycleState::Published);
}

#[test]
fn a_plan_subject_with_no_pinned_lifecycle_state_is_refused_rather_than_read_live() {
    // Read live, the row's state admits `superseded` - a third value D-128 does
    // not contemplate for a projected subject, and one a consumer coding
    // sellability predicate (4) as "is published" reads as unsellable. Frozen
    // into an INSERT-only delta that is permanent, so the absence fails closed.
    let mut row = ref_row("pend-x", &SubjectRef::Plan(Uuid::from_u128(1)));
    row.subject_lifecycle_state = None;

    let refusal = subject_of(&row).expect_err("a plan subject pins the state it judged");
    match refusal {
        DomainError::Internal(message) => assert!(
            message.contains("no lifecycle state"),
            "the refusal must say what is missing, got: {message}"
        ),
        other => panic!("expected an internal fault, got {other:?}"),
    }
}

#[test]
fn a_plan_subject_with_no_pinned_revision_is_refused_rather_than_defaulted() {
    // Defaulting to the current revision is the defect the column closes: it
    // is wrong exactly when a second publish beat the warm, and the wrong
    // answer freezes permanently into a store whose contract is that a
    // completed version never changes.
    let mut row = ref_row("pend-x", &SubjectRef::Plan(Uuid::from_u128(1)));
    row.subject_revision = None;

    let refusal = subject_of(&row).expect_err("a plan subject pins the revision it judged");
    match refusal {
        DomainError::Internal(message) => assert!(
            message.contains("no revision"),
            "the refusal must say what is missing, got: {message}"
        ),
        other => panic!("expected an internal fault, got {other:?}"),
    }
}
