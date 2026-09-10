//! The three pure pieces of the workflow, proven where they are pure.
//!
//! The storage-backed half — that a real pending unit is really voided by a real
//! mutation, that a self-approval really lands a `deny` row — is
//! `tests/sqlite_approval_service.rs`, where there is a database to put rows in.
//! What is here is what needs none: how a subject ref resolves to a plan, which
//! regions a change set touches, and which wire refusal each judgement becomes.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::BTreeSet;

use serde_json::json;
use uuid::Uuid;

use super::{
    ApproverReach, RegionGrant, approver_reach, independent_approver, refusal_to_domain, regions_of,
};
use crate::domain::approval::{
    ApprovalState, DecisionBy, DecisionRefusal, DecisionRequest, WithdrawAuthority,
    authorize_decision,
};
use crate::domain::audit::AuditSubjectKind;
use crate::domain::concurrency::RowVersion;
use crate::domain::contracts::{BillingAnchorPolicy, ProrationBasis, ProrationContract};
use crate::domain::error::DomainError;
use crate::domain::instant::utc_ymd_hms;
use crate::domain::lifecycle::LifecycleState;
use crate::domain::money::CurrencyCode;
use crate::domain::plan_shape::PlanShape;
use crate::domain::price_record::PriceRecord;
use crate::domain::price_row::{ModelKind, PriceRow};
use crate::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use crate::infra::storage::repo::approval_repo::ApprovalRecord;

const PLAN: Uuid = Uuid::from_u128(0x91a4);

fn record_with_subject(subject_ref: &str) -> ApprovalRecord {
    ApprovalRecord {
        approval_id: Uuid::from_u128(0xa1),
        tenant_id: Uuid::from_u128(0x7e11),
        subject_ref: subject_ref.to_owned(),
        subject_kind: AuditSubjectKind::PlanRevision,
        content_hash: vec![0xde, 0xad],
        state: ApprovalState::Submitted,
        submitter_principal: Uuid::from_u128(0x5b01),
        approver_principal: None,
        reason: None,
        materiality: json!({}),
        submitted_at: utc_ymd_hms(2026, 8, 3, 9, 0, 0),
        decided_at: None,
    }
}

// ---------------------------------------------------------------------------
// Which plan a unit is about
// ---------------------------------------------------------------------------

/// The ref `audit_repo::plan_revision_ref` mints round-trips.
///
/// Written against a ref that function **produced**, not against a literal: the
/// two are one contract, and a literal here would keep passing the day the
/// renderer changed its separator.
/// Through `approval_repo::subject_plan` — the **parse**, which is what stayed pure
/// when `plan_of` grew a store read. A `window` subject's plan comes off the
/// window's scope key rather than out of its ref, so the resolution as a whole now
/// needs a runner and is asserted in `tests/sqlite_window_service.rs`; the parse
/// does not, and this is it.
#[test]
fn a_plan_revision_ref_resolves_to_its_plan() {
    let plan = PlanId::new(PLAN);
    let rendered = crate::infra::storage::repo::audit_repo::plan_revision_ref(plan, 7);
    assert_eq!(
        crate::infra::storage::repo::approval_repo::subject_plan(&record_with_subject(&rendered))
            .unwrap(),
        plan
    );
}

/// A ref this crate cannot have written is an invariant breach, not a caller's
/// mistake: the CHECKs admit any text, so an unparseable ref means the table was
/// written around.
///
/// **Both halves**: the parse answers `CorruptRow`, and the rendering an operator
/// meets is a 500 rather than a 400. The second assertion is what makes this test
/// about the *classification* — a `CorruptRow` quietly re-mapped onto the
/// bad-request family would tell a caller to fix a request they cannot fix.
#[test]
fn a_subject_ref_that_is_not_a_plan_revision_is_an_internal_fault() {
    for malformed in ["", "not-a-uuid/3", "3", &PLAN.to_string()] {
        let err = crate::infra::storage::repo::approval_repo::subject_plan(&record_with_subject(
            malformed,
        ))
        .expect_err("this is not a plan revision ref");
        assert!(
            matches!(err, crate::infra::storage::RepoError::CorruptRow(_)),
            "{malformed:?} gave {err:?}"
        );
        assert!(
            matches!(
                crate::infra::storage::repo_failure(&err),
                DomainError::Internal(_)
            ),
            "{malformed:?} must reach the wire as a 500, not a 400"
        );
    }
}

// ---------------------------------------------------------------------------
// Which regions a change set touches
// ---------------------------------------------------------------------------

fn row(market: &str) -> PriceRecord {
    let scope_key = ScopeKey::new(
        PlanId::new(PLAN),
        CurrencyCode::new("USD").expect("three letters"),
        Region::new(market).expect("a non-blank region"),
        PhaseId::new(Uuid::from_u128(0x11)),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");
    PriceRecord {
        price_id: Uuid::new_v4(),
        scope_key,
        row: PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat)),
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: None,
        // Stated, because this is a **recurring** row and Slice 6's
        // `inst-pi-required` makes the three proration inputs mandatory on one.
        // A fixture that asserts a clean publish needs a row publishable in every
        // respect but the one under judgement, and a row with no proration
        // contract is not.
        proration_contract: Some(ProrationContract {
            billing_anchor_policy: BillingAnchorPolicy::CalendarMonth,
            proration_basis: ProrationBasis::CalendarDaysActual,
            credit_on_downgrade: false,
        }),
        rounding_policy_ref: None,
        grandfather_until: None,
        supersedes_price_id: None,
        lifecycle_state: LifecycleState::Draft,
        created_by: Uuid::from_u128(0xac10),
        created_at_utc: utc_ymd_hms(2026, 8, 3, 9, 0, 0),
        row_version: RowVersion::new(0),
    }
}

fn shape_over(markets: &[&str]) -> PlanShape {
    let mut shape = PlanShape::new(PlanId::new(PLAN), 3, utc_ymd_hms(2026, 8, 3, 12, 0, 0));
    shape.rows = markets.iter().map(|market| row(market)).collect();
    shape
}

#[test]
fn the_change_sets_regions_are_the_union_over_its_rows() {
    let regions = regions_of(&shape_over(&["EU", "US", "EU"]));
    let expected: BTreeSet<Region> = ["EU", "US"]
        .iter()
        .map(|value| Region::new(value).expect("a non-blank region"))
        .collect();
    assert_eq!(regions, expected);
}

/// A pure plan-shape revision touches no region at all.
///
/// Not a degenerate case: D-115 makes shape-only revisions — a phase duration, a
/// GL code, an add-on rule — always material, so they reach the approval
/// workflow routinely and carry zero price rows when they do.
#[test]
fn a_revision_with_no_price_rows_touches_no_region() {
    assert!(regions_of(&shape_over(&[])).is_empty());
}

// ---------------------------------------------------------------------------
// Which set the scope rule measures the approver against
// ---------------------------------------------------------------------------

/// **An untransported grant resolves to no measurement**, which is a different
/// answer from a covering one and has to stay distinguishable from it.
///
/// [`judge`](super::judge) does synthesise the change set from here — D2 keeps
/// that, because fail-closed refuses every change set carrying one price row —
/// and the synthesis is at a named arm in `judge` rather than inside
/// [`approver_reach`]. So what this pins is the resolver answering *"there was
/// nothing to measure"*: a resolver answering [`ApproverReach::Granted`] here
/// would make the unenforced case indistinguishable from an approver who holds
/// every region the change touches, to a reader and to the alarm
/// `api::rest::approvals::report_region_grant_transport` raises.
#[test]
fn an_untransported_grant_is_not_a_measurement() {
    assert_eq!(
        approver_reach(&RegionGrant::Untransported),
        ApproverReach::Unmeasured
    );
}

/// An explicit grant is taken exactly as given, and **a narrower one refuses when
/// the rule runs on it**.
///
/// Both halves in one case, through the two steps `judge` runs — resolve the
/// grant, then judge with what came back — because either half alone is satisfied
/// by a broken other. `decision_tests` drives `authorize_decision` against sets
/// handed to it directly, so a resolver that widened an explicit grant, or
/// answered [`ApproverReach::Unmeasured`] for one, would leave every case there
/// green while no caller of this crate could ever be refused. Asserting
/// `!change_set.is_subset(&narrow)` instead would be set algebra over two
/// literals: true whatever the rule does, and true if the rule is deleted.
///
/// The covering grant is the positive control: without it a resolver returning
/// the empty set for everything refuses this case too, and `OutOfScope` would be
/// what this test reports for every input.
#[test]
fn an_explicit_grant_is_taken_exactly_as_given_and_a_narrow_one_refuses() {
    let change_set = regions_of(&shape_over(&["EU", "US"]));
    let narrow: BTreeSet<Region> = BTreeSet::from([Region::new("EU").expect("a region")]);

    assert_eq!(
        approver_reach(&RegionGrant::Explicit(narrow.clone())),
        ApproverReach::Granted(narrow.clone())
    );
    assert_eq!(
        judged_with(&RegionGrant::Explicit(narrow), &change_set),
        Err(DecisionRefusal::OutOfScope),
        "a grant missing a region the change set touches is inst-ap-scope's refusing direction"
    );

    let covering: BTreeSet<Region> = change_set.clone();
    assert_eq!(
        judged_with(&RegionGrant::Explicit(covering), &change_set),
        Ok(()),
        "and the same path authorizes a grant that covers it, or the refusal above is \
         whatever this returns for every input"
    );
}

/// Resolve a grant and judge with what came back — the scope rule's two steps,
/// composed as [`judge`](super::judge) composes them.
///
/// Every field but the two region sets is fixed at a value the earlier rules
/// pass, so a refusal out of here is `inst-ap-scope`'s and cannot be pendingness,
/// self-approval or a content mismatch wearing its name.
fn judged_with(
    grant: &RegionGrant,
    change_set_regions: &BTreeSet<Region>,
) -> Result<(), DecisionRefusal> {
    let approver_regions = match approver_reach(grant) {
        ApproverReach::Unmeasured => change_set_regions.clone(),
        ApproverReach::Granted(regions) => regions,
    };
    let pinned = [7_u8; 32];
    authorize_decision(&DecisionRequest {
        record_state: ApprovalState::Submitted,
        submitter_principal: Uuid::from_u128(0x5ab),
        decision: DecisionBy::Approve(Uuid::from_u128(0xa99)),
        reason: None,
        pinned_content_hash: &pinned,
        current_content_hash: Some(pinned),
        approver_regions: &approver_regions,
        change_set_regions,
        withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
    })
}

// ---------------------------------------------------------------------------
// Which wire refusal each judgement becomes
// ---------------------------------------------------------------------------

/// Every refusal maps to a distinct domain variant, and the detail names the
/// record.
///
/// The mapping is exhaustive by construction — `refusal_to_domain` matches on
/// the enum — but *which* variant each becomes is a classification decision
/// (403 vs 409 vs 400), and this is where it is written down once.
#[test]
fn each_refusal_becomes_the_variant_its_status_needs() {
    let id = Uuid::from_u128(0xa1);
    for refusal in DecisionRefusal::ALL {
        let err = refusal_to_domain(*refusal, id);
        let matched = matches!(
            (refusal, &err),
            (
                DecisionRefusal::SelfApproval,
                DomainError::SelfApprovalForbidden(_)
            ) | (
                DecisionRefusal::OutOfScope,
                DomainError::RegionScopeDenied(_)
            ) | (
                DecisionRefusal::ContentMismatch,
                DomainError::ApprovalContentMismatch(_)
            ) | (
                DecisionRefusal::NotPending,
                DomainError::ApprovalNotPending(_)
            ) | (
                DecisionRefusal::ReasonRequired,
                DomainError::ReasonRequired(_)
            ) | (
                DecisionRefusal::ForeignWithdraw,
                DomainError::WithdrawForbidden(_)
            )
        );
        assert!(matched, "{refusal:?} became {err:?}");
        assert!(
            err.to_string().contains(&id.to_string()),
            "the refusal must name the record: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// Which approved records actually authorize
// ---------------------------------------------------------------------------

/// An approved record built to order, so the two invariant breaches below can be
/// staged at all.
///
/// **They cannot be staged through the store, and that is the point of testing the
/// predicate rather than a route.** `chk_pricing_approval_distinct_principals`
/// (`approver_principal IS NULL OR approver_principal <> submitter_principal`) and
/// `chk_pricing_approval_approver` (`state IN ('submitted','voided') OR
/// approver_principal IS NOT NULL`) are in both backends, so no writer in this crate
/// can produce either row. What this guards is the case those CHECKs did not run in
/// — a migration, a restore, a later slice's writer — which is exactly the argument
/// `authorization_of` already carries for the same rule on the publish path.
fn approved_by(submitter: Uuid, approver: Option<Uuid>) -> ApprovalRecord {
    ApprovalRecord {
        state: ApprovalState::Approved,
        submitter_principal: submitter,
        approver_principal: approver,
        decided_at: Some(utc_ymd_hms(2026, 8, 3, 10, 0, 0)),
        ..record_with_subject("019fd000-0000-7000-8000-000000000000/0")
    }
}

/// The ordinary case: two principals, so the record authorizes and names the second.
#[test]
fn an_approved_record_decided_by_a_second_principal_authorizes() {
    let submitter = Uuid::from_u128(0x5b01);
    let approver = Uuid::from_u128(0xa99a);

    let answer = independent_approver(&approved_by(submitter, Some(approver)));

    assert_eq!(answer.expect("two principals authorize"), approver);
}

/// One principal twice authorizes nothing — the whole of `inst-tp-distinct`.
#[test]
fn an_approved_record_naming_one_principal_twice_authorizes_nothing() {
    let one = Uuid::from_u128(0x5b01);

    let answer = independent_approver(&approved_by(one, Some(one)));

    assert!(
        matches!(answer, Err(DomainError::SelfApprovalForbidden(_))),
        "a record naming {one} as both submitter and approver is not a second signature: {answer:?}"
    );
}

/// An `approved` record with no approver at all is an invariant breach, not an
/// approval — and it is a *different* answer from the self-approval above, because
/// the two name different faults to whoever reads the log.
#[test]
fn an_approved_record_with_no_approver_is_an_invariant_breach() {
    let answer = independent_approver(&approved_by(Uuid::from_u128(0x5b01), None));

    assert!(
        matches!(answer, Err(DomainError::Internal(_))),
        "an approved record carrying no approver is a broken invariant: {answer:?}"
    );
}
