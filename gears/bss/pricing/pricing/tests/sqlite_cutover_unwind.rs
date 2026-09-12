//! D-05's retirement unwind, held open by probes rather than by prose.
//!
//! `inst-co-retirement-unwind` (Slice 7 step 7) and `inst-rt-cancel` (Slice 11
//! step 2) require that a plan retirement **unwind a live cutover unit in the
//! same transaction**: the predecessor window's `effectiveTo` restored to its
//! recorded pre-cutover value, the scheduled copy and successor windows
//! cancelled, the unit closed as `unwound` — a merely `submitted` unit voided per
//! the Slice 5 pin semantics — and the whole act registered always-material.
//!
//! None of the first four is built. That much was already on record in one place
//! (Slice 11's AC list, *"Slice 7's `inst-co-retirement-unwind` has no
//! implementation"*) and absent from the one list whose job is to name what
//! `retire_in` does not do. What was on record **nowhere** is the part this file
//! exists for: two of D-05's clauses were not merely unbuilt, they were not
//! **constructible** in this crate as it stood, and each was blocked on a
//! different missing thing. **One of the two was built on 2026-08-17** and this
//! file is now the account of both halves.
//!
//! * The predecessor's pre-cutover `effectiveTo` was **overwritten in place** by
//!   the act that would have to be undone, with no row, no event and no audit
//!   entry preserving it. It is recorded now:
//!   `a_cutover_records_the_effective_to_it_overwrites` reads the value back out
//!   of the audit pair through `audit_repo::for_subject`, and
//!   `the_audit_before_image_agrees_with_the_pre_cutover_frozen_delta` checks it
//!   against the one other store that independently holds the same interval.
//!   Two cases hold the **restore** the operand exists for:
//!   `the_append_only_trigger_does_not_refuse_the_unwinds_restore` and
//!   `an_open_ended_predecessor_is_recorded_as_null_and_restorable_to_null`.
//! * `unwound` is not a state this crate's approval machine has, and it is not a
//!   state the **design set** declares either: `05-governance.md` §4's approval
//!   state machine lists `submitted, approved, rejected, voided` and §6's `state`
//!   column repeats exactly those four. Minting a fifth here is what D-204 clause (2)
//!   refuses — *"a gear may mint an internal variant freely; a wire code is the
//!   set's to declare"* — which
//!   `domain::retirement::strand_free_disposition` already cites from the other
//!   side. `the_approval_machine_has_no_unwound_state` holds that.
//!
//! And one clause **is** paid, under another name:
//! `a_retirement_over_a_live_cutover_is_material_under_the_trigger_it_already_declares`
//! shows that D-05's always-material half needs no
//! `Trigger::RetirementUnwindingACutover` at all, because D-109 made retirement
//! unconditionally material and both declarations answer under the same rule.
//!
//! **The reason that sentence used to give stopped being true on 2026-08-16.** It
//! read *"and `MaterialityVerdict` carries no trigger identity for the two to
//! differ in"*, which is how D-327 clause (4) argued the clause paid. The verdict
//! carries the trigger now, so the two declarations **are** distinguishable in the
//! stored record; what survives is that neither changes whether a second principal
//! is required. That case asserts both halves, and the difference is owed back to
//! the register rather than smoothed over here.
//!
//! # Why the whole unwind and not the three clauses that *are* constructible
//!
//! Finding the live unit, cancelling the two scheduled windows and — since the
//! operand landed — restoring the bound off the trail can all be written today.
//! Writing any proper subset produces exactly the state D-05 rejected option (a)
//! to avoid: a predecessor still shortened to the cutover instant with the
//! schedules that were to take over from it gone — the trailing void no gap check
//! can see, arriving as the *result* of the fix. A half unwind is worse than
//! none, so this file records the whole of it as absent.
//!
//! **While it is absent the retirement is refused, not silently performed**, and
//! `a_retirement_over_a_live_cutover_is_refused` is that case: the refusal lands
//! before the flip, so the window plane, the approval plane and the plan's own state
//! are all exactly where the cutover left them.
//! `infra::retirement::ensure_no_live_cutover` carries the argument for the arm, and
//! it is that case which flips the day the unwind lands.
//!
//! That refusal carries `RETIRE_OVER_LIVE_CUTOVER`, a **wire code this gear mints**
//! and whose §5 entry is owed — which is a deviation from the same D-204 clause (2)
//! quoted above and not a contradiction of it: `unwound` would be a state the
//! approval machine has to hold and has no producer, where this is a discriminator
//! on a refusal already reaching a caller. `domain::retirement::RETIRE_OVER_LIVE_CUTOVER`
//! states the cost.
//!
//! # Every instant is a fixed date
//!
//! `sqlite_cutover_unit`'s rule and its fixtures: the world sits inside `common`'s
//! coverage window `[2099-08-04, 2099-09-01)`, the cutover is at 2099-08-20, and
//! "now" is a constant. The predecessor's coverage window therefore ends at a
//! **finite** instant, which is the whole point of the first case: after the
//! shorten there is no way to tell that window from one that had been open-ended,
//! and `compose_cutover_windows` accepts both (`WindowInterval::covers` is
//! satisfied by an absent end and by any end strictly after the cutover alike).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::StatusCode;
use bss_pricing::api::rest::retirement::PLAN_RETIRE;
use bss_pricing::config::JobsConfig;
use bss_pricing::config::LimitsConfig;
use bss_pricing::domain::approval::ApprovalState;
use bss_pricing::domain::audit::AuditSubjectKind;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::materiality::triggers::Trigger;
use bss_pricing::domain::materiality::{self, ChangeSet, MaterialityReason, MaterialityVerdict};
use bss_pricing::domain::money::MinorAmount;
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::scope_key::{PlanId, ScopeKey};
use bss_pricing::domain::sellability::SellabilityFacts;
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::cutover::{
    CutoverOutcome, CutoverReceipt, CutoverRequest, CutoverService, cutover_unit_ref,
};
use bss_pricing::infra::fixture_gate::FixtureGate;
use bss_pricing::infra::jobs::readmodel_warm::ReadModelWarmJob;
use bss_pricing::infra::retirement::{RetirementOutcome, RetirementService};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::entity::{audit_log, outbox, price_window};
use bss_pricing::infra::storage::repo::{
    PendingVersionRow, approval_repo, audit_repo, window_repo,
};
use time::OffsetDateTime;

use bss_pricing::domain::instant::format_rfc3339;
use rest_support::{Harness, Publishable, problem_code, request};
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x_5c_13);
const REVIEWER: Uuid = Uuid::from_u128(0x_5c_23);
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_5c_c2);

fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 5, 0, 0, 0)
}

/// Inside the predecessor's coverage and clear of both changeover floors.
fn cutover_at() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

/// The instant the fixture's coverage window ends at, and the one the cutover
/// overwrites — `common::COVERAGE_TO_UTC` at midnight.
///
/// Named here rather than reached through `common` so the assertion below says
/// what it is looking for; `common`'s own doc makes the same point in reverse
/// about not re-spelling the constant at a call site, and this **is** the call
/// site that has to name the value as a value.
fn pre_cutover_effective_to() -> OffsetDateTime {
    utc_ymd_hms(2099, 9, 1, 0, 0, 0)
}

/// The instant the fixture's coverage window **begins** — `common::COVERAGE_FROM_UTC`
/// at midnight.
///
/// [`pre_cutover_effective_to`]'s sibling and named here for its reason. It is the
/// other half of the audit record's identity for this window, and the half that can
/// never move: the append-only trigger's arm 4 refuses any `UPDATE` that changes
/// `effective_from`, which is exactly why the record can be keyed on it.
fn predecessor_effective_from() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 4, 0, 0, 0)
}

fn stamp_of(actor: Uuid) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: now(),
        correlation_id: TEST_CORRELATION,
    }
}

async fn published_plan(h: &Harness) -> (PlanId, Publishable) {
    let plan_uuid = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(h, plan_uuid).await;
    h.publish(plan_uuid, seeded.revision).await;
    h.publish_price(plan_uuid, seeded.price_id).await;
    (PlanId::new(plan_uuid), seeded)
}

fn key_of(plan_id: PlanId, seeded: &Publishable) -> ScopeKey {
    rest_support::publishable_scope_key(plan_id, seeded.phase, "eu")
}

fn successor_content(amount: i64) -> PriceContent {
    let mut content = rest_support::publishable_row();
    content.row.amount_minor = Some(MinorAmount::new(amount).expect("non-negative"));
    content
}

fn request_of(key: &ScopeKey, amount: i64) -> CutoverRequest {
    CutoverRequest {
        predecessor_key: key.clone(),
        cutover_at: cutover_at(),
        successor: successor_content(amount),
        successor_price_id: Uuid::now_v7(),
        successor_window_id: Uuid::now_v7(),
        copy_price_id: Uuid::now_v7(),
        copy_window_id: Uuid::now_v7(),
        reason_code: "grandfatheringCutover".to_owned(),
    }
}

async fn cut_over(
    h: &Harness,
    request: CutoverRequest,
    actor: Uuid,
) -> Result<CutoverOutcome, DomainError> {
    CutoverService::new(
        h.db.clone(),
        &LimitsConfig::default(),
        FixtureGate::load(&rest_support::committed_registry_path()),
        Arc::clone(&h.registry) as Arc<_>,
    )
    .cut_over(
        &rest_support::security_context(actor, h.tenant),
        &h.scope(),
        h.tenant,
        request,
        bss_pricing::api::rest::windows::verdict_json,
        stamp_of(actor),
    )
    .await
}

async fn retire(h: &Harness, plan_id: PlanId) -> Result<RetirementOutcome, DomainError> {
    RetirementService::new(h.db.clone(), Arc::clone(&h.registry) as Arc<_>)
        .retire(
            &rest_support::security_context(SUBMITTER, h.tenant),
            &h.scope(),
            h.tenant,
            plan_id,
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(SUBMITTER),
        )
        .await
}

async fn approve(h: &Harness, approval_id: Uuid) {
    use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
    use bss_pricing::infra::approval::{DecideRequest, RegionGrant};
    h.governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            DecideRequest {
                approval_id,
                decision: DecisionBy::Approve(REVIEWER),
                reason: None,
                approver_regions: RegionGrant::Untransported,
                stamp: stamp_of(REVIEWER),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer approves the unit");
}

/// Submit, approve, re-post: one committed cutover on the seeded key.
async fn committed_cutover(h: &Harness, key: &ScopeKey) -> CutoverReceipt {
    let opened = cut_over(h, request_of(key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let approval_id = match &opened {
        CutoverOutcome::SubmittedForApproval(pending) => pending.approval.approval_id,
        CutoverOutcome::Committed(_) => panic!("the first call must open a unit"),
    };
    approve(h, approval_id).await;
    match cut_over(h, request_of(key, 12_000), SUBMITTER)
        .await
        .expect("the approved cutover commits")
    {
        CutoverOutcome::Committed(receipt) => *receipt,
        CutoverOutcome::SubmittedForApproval(_) => panic!("the second call must commit"),
    }
}

/// Submit, approve, re-post: one committed retirement of the plan.
async fn committed_retirement(h: &Harness, plan_id: PlanId) {
    let opened = retire(h, plan_id).await.expect("the retirement unit opens");
    let approval_id = match &opened {
        RetirementOutcome::SubmittedForApproval(pending) => pending.approval.approval_id,
        RetirementOutcome::Retired(_) => {
            panic!("D-109 makes retirement always material; the first call must open a unit")
        }
    };
    approve(h, approval_id).await;
    match retire(h, plan_id)
        .await
        .expect("the approved retirement commits")
    {
        RetirementOutcome::Retired(_) => (),
        RetirementOutcome::SubmittedForApproval(_) => panic!("the second call must commit"),
    }
}

/// Every JSON artefact the tenant carries, keyed by the row's own identity — the
/// outbox payloads by `outbox_id`, the audit records by `(chain_id, seq)`.
///
/// **Identities and not a high-water mark**, because `seq` is not a global
/// sequence in either table: `audit_repo::page`'s doc states it counts *within a
/// segment*, so `seq > floor` over a tenant would silently drop the rows of a
/// chain that happens to be behind. Diffing the identity sets is exact and says
/// nothing about ordering.
///
/// Rendered as text rather than walked field by field, because the claim is about
/// the **value** and not about any field name: an operand that survived under a
/// name nobody guessed would still be an operand, and this search would find it.
async fn json_artefacts(h: &Harness) -> Vec<(String, String)> {
    let conn = h.db.conn().expect("conn");
    let mut rendered = Vec::new();
    for row in outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(outbox::Column::TenantId.eq(h.tenant)))
        .all(&conn)
        .await
        .expect("read the outbox")
    {
        rendered.push((
            format!("outbox:{}", row.outbox_id),
            format!("outbox {}: {}", row.event_name, row.payload),
        ));
    }
    for row in audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(audit_log::Column::TenantId.eq(h.tenant)))
        .all(&conn)
        .await
        .expect("read the audit log")
    {
        rendered.push((
            format!("audit:{}:{}", row.chain_id, row.seq),
            format!(
                "audit {} {}: before={} after={}",
                row.action,
                row.subject_ref,
                row.before_state.unwrap_or(serde_json::Value::Null),
                row.after_state.unwrap_or(serde_json::Value::Null)
            ),
        ));
    }
    rendered
}

/// The artefacts present after the act and absent before it.
fn written_by_the_act(before: &[(String, String)], after: Vec<(String, String)>) -> Vec<String> {
    let seen: BTreeSet<&str> = before.iter().map(|(id, _)| id.as_str()).collect();
    after
        .into_iter()
        .filter(|(id, _)| !seen.contains(id.as_str()))
        .map(|(_, rendered)| rendered)
        .collect()
}

/// Every window row of the tenant, rendered.
async fn window_rows(h: &Harness) -> Vec<price_window::Model> {
    let conn = h.db.conn().expect("conn");
    price_window::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(price_window::Column::TenantId.eq(h.tenant)))
        .all(&conn)
        .await
        .expect("read the window plane")
}

// ---------------------------------------------------------------------------
// (1) The operand D-05 calls "its recorded pre-cutover value".
// ---------------------------------------------------------------------------

/// D-05's first clause names a value, and the act that destroys it records it.
///
/// **This case is the inversion of the one it replaces.** Until 2026-08-17 it was
/// `a_cutover_records_the_effective_to_it_overwrote_nowhere` and its final
/// assertion was that *nothing* the act wrote carried the pre-cutover instant —
/// with the standing instruction that *"if something here now carries it, the
/// operand exists and this case is the one to delete"*. It does, so this is that
/// deletion, performed by turning the search around rather than by dropping it:
/// the same three stores are read the same way, and the claim on the last one is
/// negated.
///
/// `commit_cutover` still shortens through `window_repo::adjust_effective_to`, an
/// `UPDATE` of `pricing_price_window.effective_to` over a table with no
/// before-image column, and `cutover_in`'s three events are still none of them
/// about the shorten. What changed is `record_cutover`: `cutover_state` now carries
/// `shortenedWindow`, the interval of the window the act shortens, on **both** sides
/// of the audit pair.
///
/// # Why the value and not a digest, and why keyed by `(scopeKey, effectiveFrom)`
///
/// The value, because `compose_cutover_windows` picks the window
/// `WindowInterval::covers(cutover)` answers `true` for, which is satisfied by an
/// absent end *and* by any end strictly after the cutover; both are legal
/// predecessors and after the shorten they are the same row. So the pre-cutover end
/// is not reconstructible from the shape, and a hash of it would let a reader verify
/// a guess without producing one. An unwind that guessed `None` would hand the
/// predecessor open-ended coverage it never had — inventing coverage on a plan being
/// retired, the one direction nothing here may move in.
///
/// Keyed by the scope key and the start, because the record's `subject_ref` is the
/// **act** (`cutover_unit_ref`: plan, key-set hash, instant) and names no window at
/// all. `effectiveFrom` is the only column of that row the append-only trigger
/// freezes outright, so it is the one identifier that still names this interval after
/// the act has moved the other end.
///
/// # It is read the way an unwind would read it
///
/// Through `audit_repo::for_subject` under a `subject_ref` **rebuilt from the plan,
/// the key and the instant** rather than remembered from the receipt — which is the
/// property that makes the operand reachable at all: a retirement holds the plan and
/// can find the live unit's subject, and does not hold a chain id or a `seq`. The
/// tenant-wide `page` walk is what the previous version of this case had to use, and
/// a reader that has to scan a tenant's whole trail to find one act's before-image is
/// not a reader.
///
/// The positive control is the `after_state` half: the same field on the same record
/// carries the cutover instant, so a green here is the operand being present and not
/// the assertion reading a field that is absent on both sides.
#[tokio::test]
async fn a_cutover_records_the_effective_to_it_overwrites() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    // The fixture's coverage window is finite, and this is the value under test.
    let before = window_rows(&h).await;
    assert!(
        before
            .iter()
            .any(|row| row.effective_to == Some(pre_cutover_effective_to())),
        "the fixture's predecessor window must end at a finite instant, or this case is \
         asserting about a value that was never there: {before:?}"
    );

    let before_the_act = json_artefacts(&h).await;
    let receipt = committed_cutover(&h, &key).await;

    // Positive control on the write itself: the shorten happened, and the truth side
    // no longer holds the old end anywhere.
    let after = window_rows(&h).await;
    let shortened = after
        .iter()
        .find(|row| row.window_id == receipt.shortened_window_id)
        .expect("the predecessor's window is still there");
    assert_eq!(
        shortened.effective_to,
        Some(cutover_at()),
        "the act moved the predecessor's end to the cutover"
    );
    assert!(
        !after
            .iter()
            .any(|row| row.effective_to == Some(pre_cutover_effective_to())),
        "and no window row carries the old end any more, which is what makes the trail the only \
         place it can be: {after:?}"
    );

    let artefacts = written_by_the_act(&before_the_act, json_artefacts(&h).await);
    assert!(
        !artefacts.is_empty(),
        "the act must have written something, or the search below proves nothing"
    );
    let lost = "2099-09-01T00:00:00";
    assert!(
        artefacts.iter().any(|rendered| rendered.contains(lost)),
        "something the act wrote carries the end it overwrote. This is the assertion that used to \
         read `is_empty()`, and the sentence beside it said that if it ever stopped holding, the \
         operand existed: {artefacts:?}"
    );

    // And now the part that makes it an operand rather than a coincidence: it is
    // addressable under the act's own subject, rebuilt rather than remembered.
    let unit_ref = cutover_unit_ref(plan_id, std::slice::from_ref(&key), cutover_at());
    let records = audit_repo::for_subject(
        &h.db.conn().expect("conn"),
        &h.scope(),
        h.tenant,
        AuditSubjectKind::PriceUnit,
        &unit_ref,
    )
    .await
    .expect("read the act's records");
    // **Three records stand under one subject**, and that is the arrangement rather
    // than a surprise: the unit's opening, the independent approve and the commit all
    // carry the act's own `subject_ref`, which is what makes `approval_ref` on the last
    // one join to the first two rather than point sideways.
    // `sqlite_supersession_unit::an_approved_unit_commits_all_four_writes_and_announces_two_events`
    // asserts that trail for the sibling act. So the commit is selected by its action,
    // not by position - and the whole trail is asserted, because a probe that took
    // `records[0]` would silently be reading the submit's approval-state pair and
    // finding no window on it for a reason that has nothing to do with this fix.
    assert_eq!(
        records
            .iter()
            .map(|row| row.action.as_str())
            .collect::<Vec<_>>(),
        vec!["submit", "approve", "publish"],
        "one act, one subject, three records - and `for_subject` returns them in commit \
         order: {records:?}"
    );
    let record = records
        .iter()
        .find(|row| row.action == "publish")
        .expect("the commit's own record");

    let before_state = record
        .before_state
        .as_ref()
        .expect("the act's record carries a before-image");
    let after_state = record.after_state.as_ref().expect("and an after-image");

    assert_eq!(
        before_state["shortenedWindow"],
        serde_json::json!({
            "scopeKey": key.to_string(),
            "effectiveFrom": format_rfc3339(predecessor_effective_from()),
            "effectiveTo": format_rfc3339(pre_cutover_effective_to()),
            "state": "scheduled",
        }),
        "D-05's operand, as a value: the interval the shorten found, named by the two things a \
         reader holding only the act can match on. {before_state}"
    );

    // The positive control on the read: the same field, same record, carries what the
    // act produced. A `before_state` assertion alone would pass against a record that
    // rendered the field from the post-write row on both sides.
    assert_eq!(
        after_state["shortenedWindow"],
        serde_json::json!({
            "scopeKey": key.to_string(),
            "effectiveFrom": format_rfc3339(predecessor_effective_from()),
            "effectiveTo": format_rfc3339(cutover_at()),
            "state": "scheduled",
        }),
        "and the pair differs in exactly the one field the act moved: {after_state}"
    );
}

/// The append-only trigger does **not** bound D-05's restore, and the reason is
/// pinned here rather than argued in a comment.
///
/// # The claim
///
/// `pricing_price_window` arm 5 (Postgres twin in `pricing_price_window`) refuses any move
/// of `effective_to` where `OLD` or `NEW` is non-NULL and `<= now()`. Read cold that
/// looks like it forbids putting a bound back. It does not, inside D-05's scope, and
/// the argument has three legs:
///
/// 1. the cutover is **approved and not yet effective**, so `OLD` is the cutover
///    instant `T`, and `T > now`;
/// 2. `NEW` is the original bound, which the cutover *shortened* — so it is strictly
///    later than `T`, hence `> T > now`;
/// 3. a NULL original bound skips the clause entirely (its own case, below).
///
/// A comment asserting that is a claim. This is the measurement, and it is armed so
/// that a change to any leg reddens it: the two instants are asserted to stand in the
/// order the argument needs **before** the write is attempted, and a control move to
/// an instant in the past is asserted to be refused, so a green cannot be the trigger
/// having been dropped or the clause having stopped firing.
///
/// # What *does* stand in the restore's way, which is not the trigger
///
/// `adjust_effective_to` runs `refuse_overlap` over `OCCUPYING_STATES`, and the
/// successor's `[cutover, …)` is on the predecessor's own key — so lengthening the
/// predecessor past the cutover while that window is still `scheduled` is
/// `WINDOW_OVERLAP`. The cancel therefore has to come first, and that ordering is
/// forced by a rule rather than chosen; it is the reverse of the order D-05's own
/// sentence lists the clauses in. The grandfathered copy does not participate:
/// `inst-co-copy` moves it to a **new generation's** key, so it is on a different
/// plane. This case performs the cancel first for exactly that reason, and the
/// refusal it would otherwise take is asserted, so the ordering is pinned rather than
/// merely obeyed.
#[tokio::test]
async fn the_append_only_trigger_does_not_refuse_the_unwinds_restore() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);
    let receipt = committed_cutover(&h, &key).await;

    // Leg 1 and leg 2, asserted as the ordering the argument rests on rather than
    // assumed. If a fixture ever moved the cutover past the original bound, or the
    // suite's clock past the cutover, this is what would say so.
    assert!(
        cutover_at() > OffsetDateTime::now_utc(),
        "leg 1: inside D-05's scope the cutover has not yet taken effect, so OLD > now"
    );
    assert!(
        pre_cutover_effective_to() > cutover_at(),
        "leg 2: the cutover *shortened* the window, so the end it is restored to is later than \
         the cutover and therefore also in the future"
    );

    let conn = h.db.conn().expect("conn");
    // By **price id**, not by "the one at the cutover that is not the predecessor":
    // the copy's window starts at the cutover too, and it is the successor's that shares
    // the predecessor's key and therefore the one the overlap rule is about.
    let successor_window = window_rows(&h)
        .await
        .into_iter()
        .find(|row| row.price_id == receipt.successor_price_id)
        .expect("the cutover scheduled the successor's window");

    // The ordering, pinned: with the successor still scheduled on the key, the
    // restore is refused - and not by the trigger.
    let held = window_repo::find(&conn, &h.scope(), h.tenant, receipt.shortened_window_id)
        .await
        .expect("read the shortened window")
        .expect("it is there");
    let premature = window_repo::adjust_effective_to(
        &conn,
        &h.scope(),
        h.tenant,
        receipt.shortened_window_id,
        Some(pre_cutover_effective_to()),
        held.mutation_seq,
        stamp_of(SUBMITTER),
    )
    .await;
    assert!(
        matches!(premature, Err(RepoError::WindowOverlap { .. })),
        "the unwind must cancel the schedules before it restores the bound: with the successor \
         still occupying `[cutover, …)` on this key, the lengthened predecessor intersects it. \
         This is the ordering constraint, and it is not the append-only trigger: {premature:?}"
    );

    // So: cancel the successor's window, the way the unwind's second clause will.
    window_repo::transition(
        &conn,
        &h.scope(),
        h.tenant,
        successor_window.window_id,
        WindowState::Cancelled,
        now(),
        stamp_of(SUBMITTER),
    )
    .await
    .expect("a scheduled window may be cancelled");

    // The control that keeps this case honest: the clause **is** live, and fires on a
    // target that is not in the future. Without this, a green above would be
    // indistinguishable from the trigger having been dropped by a later migration.
    let held = window_repo::find(&conn, &h.scope(), h.tenant, receipt.shortened_window_id)
        .await
        .expect("read the shortened window")
        .expect("it is there");
    let into_the_past = window_repo::adjust_effective_to(
        &conn,
        &h.scope(),
        h.tenant,
        receipt.shortened_window_id,
        Some(utc_ymd_hms(2020, 1, 1, 0, 0, 0)),
        held.mutation_seq,
        stamp_of(SUBMITTER),
    )
    .await;
    assert!(
        matches!(into_the_past, Err(RepoError::WindowHistorical { .. })),
        "an end may only be moved to a future instant, and the rule that says so is live: \
         {into_the_past:?}"
    );

    // And the restore itself, which is what D-05's first clause is.
    let restored = window_repo::adjust_effective_to(
        &conn,
        &h.scope(),
        h.tenant,
        receipt.shortened_window_id,
        Some(pre_cutover_effective_to()),
        held.mutation_seq,
        stamp_of(SUBMITTER),
    )
    .await
    .expect(
        "the append-only trigger does not refuse the restore: OLD is the cutover instant and NEW \
         is the bound it shortened, so both are in the future and the clause does not apply",
    );
    assert_eq!(
        restored.effective_to,
        Some(pre_cutover_effective_to()),
        "the predecessor covers what it covered before the cutover"
    );
    assert_eq!(
        restored.effective_from, held.effective_from,
        "and the start did not move, which is why it can be the record's name"
    );
}

/// Leg 3, on its own: an **open-ended** predecessor is recorded as `null`, and the
/// restore to `null` skips the trigger's clause outright.
///
/// This is the case the operand exists for most sharply. `WindowInterval::covers`
/// admits an absent end, so an open-ended window is a legal cutover predecessor; after
/// the shorten it is byte-identical to one that had ended at some later instant. So
/// `null` here is a **positive claim** — the predecessor's coverage was unbounded —
/// and not the absence of a record, which is the distinction an unwind has to be able
/// to make and could not before.
///
/// The trigger's clause is `NEW IS DISTINCT FROM OLD AND ((NEW non-NULL AND NEW <=
/// now) OR (OLD non-NULL AND OLD <= now))`. On the restore, `NEW` is NULL so the first
/// disjunct is false, and `OLD` is the cutover instant which is in the future, so the
/// second is too: the clause does not fire at all. Pinned by performing it.
///
/// The open-ended predecessor is made by moving the fixture's own bound to NULL
/// *before* the cutover — itself a move the clause permits, for the same reason — so
/// the row under test is one this crate's own writer produced rather than one the test
/// inserted around the rules.
#[tokio::test]
async fn an_open_ended_predecessor_is_recorded_as_null_and_restorable_to_null() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);
    let conn = h.db.conn().expect("conn");

    // Open the fixture's window, through the sanctioned writer.
    let seeded_window = window_rows(&h)
        .await
        .into_iter()
        .find(|row| row.effective_to == Some(pre_cutover_effective_to()))
        .expect("the fixture's coverage window");
    window_repo::adjust_effective_to(
        &conn,
        &h.scope(),
        h.tenant,
        seeded_window.window_id,
        None,
        u64::try_from(seeded_window.mutation_seq).expect("a small counter"),
        stamp_of(SUBMITTER),
    )
    .await
    .expect("a future bound may be dropped: NEW is NULL and OLD is in the future");

    let receipt = committed_cutover(&h, &key).await;
    assert_eq!(
        receipt.shortened_window_id, seeded_window.window_id,
        "the open-ended window is the one the compose picked, since `covers` admits an absent end"
    );

    let unit_ref = cutover_unit_ref(plan_id, std::slice::from_ref(&key), cutover_at());
    let records = audit_repo::for_subject(
        &conn,
        &h.scope(),
        h.tenant,
        AuditSubjectKind::PriceUnit,
        &unit_ref,
    )
    .await
    .expect("read the act's records");
    let before_state = records
        .iter()
        .find(|row| row.action == "publish")
        .expect("the commit's own record stands under the act's subject")
        .before_state
        .as_ref()
        .expect("with a before-image");
    assert_eq!(
        before_state["shortenedWindow"]["effectiveTo"],
        serde_json::Value::Null,
        "`null` as the recorded pre-cutover value, which is a claim that the coverage was \
         unbounded and not the absence of one: {before_state}"
    );
    assert_eq!(
        before_state["shortenedWindow"]["effectiveFrom"],
        serde_json::json!(format_rfc3339(predecessor_effective_from())),
        "and the same record still names which interval it is about: {before_state}"
    );

    // The restore, with the successor out of the way as the ordering requires.
    let successor_window = window_rows(&h)
        .await
        .into_iter()
        .find(|row| row.price_id == receipt.successor_price_id)
        .expect("the successor's window");
    window_repo::transition(
        &conn,
        &h.scope(),
        h.tenant,
        successor_window.window_id,
        WindowState::Cancelled,
        now(),
        stamp_of(SUBMITTER),
    )
    .await
    .expect("a scheduled window may be cancelled");

    let held = window_repo::find(&conn, &h.scope(), h.tenant, receipt.shortened_window_id)
        .await
        .expect("read it")
        .expect("it is there");
    let restored = window_repo::adjust_effective_to(
        &conn,
        &h.scope(),
        h.tenant,
        receipt.shortened_window_id,
        None,
        held.mutation_seq,
        stamp_of(SUBMITTER),
    )
    .await
    .expect(
        "restoring to NULL skips the clause entirely: NEW is NULL and OLD is the future cutover \
         instant, so neither disjunct holds",
    );
    assert_eq!(
        restored.effective_to, None,
        "open-ended again, which is what the record said it had been"
    );
}

/// The audit before-image and the pre-cutover frozen delta **agree**.
///
/// # Why a cross-check and not a second source of truth
///
/// `pricing_read_model` already carries the pre-cutover interval: the projector's
/// `project_windows` freezes every key's window plane filtered by
/// `PROJECTED_WINDOW_STATES`, and `read_model_repo::delta_at` resolves it at a pin. So
/// after the cutover there are two independent records of what the predecessor's
/// coverage ended at — the delta frozen *before* the act, and the audit before-image
/// written *by* it — reached by different code, keyed the same way, and written for
/// different readers.
///
/// Neither is proposed as the operand. The delta is the wrong store to unwind from: it
/// is keyed by `catalog_version`, so reading it requires knowing which pin preceded
/// the act, and a plan re-projects only when a publish unit touches it — the version
/// that answers a pin is almost always an earlier one, which is `delta_at`'s own §4.4
/// rule. What the agreement buys is that a defect in either writer is visible: a
/// before-image rendered from the post-write row, or a projector filter that dropped
/// the covering window, would make the two disagree, and each alone would look fine.
///
/// # The two identifiers meet here, which is why the record is keyed as it is
///
/// The delta carries no `window_id` — `WindowInterval`'s doc argues at length that it
/// must not start — and the audit record's subject is the act and carries no window id
/// either. They are comparable only because both name the interval by `(scope key,
/// effectiveFrom)`, which is what `shortened_window_state_value` was shaped for. This
/// case is the measurement of that claim.
#[tokio::test]
async fn the_audit_before_image_agrees_with_the_pre_cutover_frozen_delta() {
    use bss_pricing::domain::read_model::SubjectRef;
    use bss_pricing::infra::storage::repo::{catalog_version_ref_repo, read_model_repo};

    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);
    let conn = h.db.conn().expect("conn");

    // Freeze the plan **before** the cutover, through the real projector: a pending
    // ref, a committed version, and the warm sweep. Nothing here hand-builds a
    // payload - the whole point is that the delta is what production code derived
    // from the truth side.
    catalog_version_ref_repo::record_pending(
        &conn,
        &h.scope(),
        PendingVersionRow::for_subject(
            h.tenant,
            "pend-before-cutover".to_owned(),
            &SubjectRef::Plan(plan_id.get()),
            Some(seeded.revision),
            Some(bss_pricing::domain::lifecycle::LifecycleState::Published),
            now(),
        ),
    )
    .await
    .expect("record the plan subject's ref");
    h.registry.commit("pend-before-cutover", 1);
    ReadModelWarmJob::new(
        h.db.clone(),
        Arc::clone(&h.registry) as Arc<dyn bss_pricing::domain::ports::CatalogVersionRegistryV1>,
        JobsConfig::default(),
    )
    .run(now())
    .await
    .expect("the warm sweep projects the plan");

    let frozen = read_model_repo::delta_at(
        &conn,
        &h.scope(),
        h.tenant,
        &SubjectRef::Plan(plan_id.get()),
        bss_pricing_sdk::CatalogVersion::new(1),
    )
    .await
    .expect("resolve the pin")
    .expect("version 1 carries this plan");

    // The interval the delta froze for the key under test, read through the gear's
    // **own** parser of that payload rather than by indexing the JSON. The two stores
    // spell a key differently on purpose — the delta writes the ten axes as an object
    // (`projection::scope_key_value`) and the audit record writes the canonical
    // rendering — so a comparison made on raw JSON would have to re-implement one of
    // them, which is the second source of truth this case exists to avoid being.
    let facts = read_model_repo::sellability_facts(&frozen).expect("a payload this gear wrote");
    let SellabilityFacts::Pinned(pinned) = facts else {
        panic!("the pin resolves, so the facts are pinned rather than unaddressable");
    };
    let frozen_group = pinned
        .windows
        .iter()
        .find(|group| group.scope_key == key)
        .unwrap_or_else(|| {
            panic!(
                "the frozen delta must carry this key's plane, or there is nothing to cross-check \
                 against: {:?}",
                pinned.windows
            )
        });
    let frozen_interval = *frozen_group
        .intervals
        .first()
        .expect("the key's coverage window");
    assert_eq!(
        frozen_interval.effective_to,
        Some(pre_cutover_effective_to()),
        "the pre-cutover pin holds the end the act is about to overwrite: {frozen_interval:?}"
    );

    let receipt = committed_cutover(&h, &key).await;
    assert_eq!(
        receipt.cutover_at,
        cutover_at(),
        "the act under test committed"
    );

    let unit_ref = cutover_unit_ref(plan_id, std::slice::from_ref(&key), cutover_at());
    let records = audit_repo::for_subject(
        &conn,
        &h.scope(),
        h.tenant,
        AuditSubjectKind::PriceUnit,
        &unit_ref,
    )
    .await
    .expect("read the act's records");
    let audited = records
        .iter()
        .find(|row| row.action == "publish")
        .expect("the commit's own record stands under the act's subject")
        .before_state
        .as_ref()
        .expect("with a before-image")["shortenedWindow"]
        .clone();

    // The agreement. The audit's `scopeKey` is asserted against the **delta's own
    // group key** rendered canonically — not against the local `key` fixture — so what
    // is measured is that the two stores are talking about one interval, rather than
    // that each independently matches a literal.
    assert_eq!(
        audited,
        serde_json::json!({
            "scopeKey": frozen_group.scope_key.to_string(),
            "effectiveFrom": format_rfc3339(frozen_interval.effective_from),
            "effectiveTo": frozen_interval.effective_to.map(format_rfc3339),
            "state": frozen_interval.state.as_str(),
        }),
        "the audit before-image and the pre-cutover frozen delta agree about the interval the \
         cutover shortened. Either alone would look fine while wrong: a before-image rendered \
         from the post-write row, or a projector filter that dropped the covering window"
    );
    // And the operand really is the destroyed one, said as a value so this case cannot
    // pass by both stores agreeing on the cutover instant.
    assert_eq!(
        audited["effectiveTo"],
        serde_json::json!(format_rfc3339(pre_cutover_effective_to())),
        "and the value they agree on is the pre-cutover end, not the one the act wrote: {audited}"
    );
}

// ---------------------------------------------------------------------------
// (2) The state D-05 closes the unit into.
// ---------------------------------------------------------------------------

/// D-05's fourth clause names an approval state neither this crate nor the design
/// set has.
///
/// `05-governance.md` §4 ("Approval State Machine") states the states as
/// *"submitted, approved, rejected, voided"* and §6's data model gives the
/// `state` column the type `submitted | approved | rejected | voided`; `ApprovalState::as_str`'s doc
/// records that those same four literals are exactly what
/// `chk_pricing_approval_state` admits. `unwound` occurs in the design set only
/// in prose — D-05's own sentence, Slice 7's step 7, Slice 11's step 2 and its AC
/// list, and PRD AC #35 — and in no state list and no column type anywhere.
///
/// So the unwind's closing move cannot be built here without minting a fifth
/// persisted **and wire** token, which is what D-204 clause (2) refuses: *"a gear
/// may mint an internal variant freely; a wire code is the set's to declare."*
/// `domain::retirement::strand_free_disposition` cites the same clause for the
/// same reason one surface over. This is a report, not a repair.
///
/// Armed forwards: add `Unwound` to `ApprovalState::ALL` and the count reddens;
/// give it the token and `from_token` reddens too. The positive control is
/// `voided`, the state D-05's *other* arm — a merely `submitted` unit — is
/// supposed to reach, which does exist and is reachable.
#[test]
fn the_approval_machine_has_no_unwound_state() {
    let tokens: Vec<&str> = ApprovalState::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        tokens,
        vec!["submitted", "approved", "rejected", "voided"],
        "the four `chk_pricing_approval_state` admits, and the four `05-governance.md` \
         section 4 declares. A fifth here is a wire token the design set has not declared \
         (D-204 (2))"
    );
    assert_eq!(
        ApprovalState::from_token("unwound"),
        None,
        "D-05 closes the unwound cutover unit as `unwound`; nothing in this machine can hold it"
    );
    assert_eq!(
        ApprovalState::from_token("voided"),
        Some(ApprovalState::Voided),
        "positive control: the state D-05's `submitted` arm reaches does exist"
    );
}

// ---------------------------------------------------------------------------
// (3) The act, as the absence it is.
// ---------------------------------------------------------------------------

/// Retiring a plan that carries an approved-not-yet-effective cutover is
/// **refused**.
///
/// `infra::retirement::ensure_no_live_cutover` is the guard, and that function
/// carries the whole argument for refusing where D-05 asks for an unwind. What this
/// case holds is the outcome and the operands: the refusal is typed, it names the
/// unit, and it lands **before** the flip, so none of the window plane, the approval
/// plane or the plan's own state has moved when it arrives. A refusal raised after
/// the flip would be the half-applied state the arm exists to avoid, and a test that
/// only read the error could not tell the two apart.
///
/// **The wire half is asserted too, and it is the half a caller branches on.** The
/// refusal is a 409 carrying `RETIRE_OVER_LIVE_CUTOVER`: the request was well formed
/// and the authority sufficient, and the remedy is to wait for the instant or
/// withdraw the unit, which is what a 409 asks for. Asserted as status **and** code,
/// because several unrelated refusals of this route share a status and the code is
/// the discriminator.
///
/// The positive control is a **second plan with no cutover**, retired in the same
/// world by the same call: without it a green here is satisfied by a retirement that
/// refuses everything.
#[tokio::test]
async fn a_retirement_over_a_live_cutover_is_refused() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let receipt = committed_cutover(&h, &key).await;
    let unit_ref = cutover_unit_ref(plan_id, std::slice::from_ref(&key), cutover_at());

    // The **first** call, and that is the claim: the guard runs before the
    // retirement's own unit is opened, so an operator is not asked for a second
    // principal on an act that cannot commit.
    let refusal = retire(&h, plan_id)
        .await
        .expect_err("a retirement over a live cutover is refused");
    let DomainError::RetireOverLiveCutover(detail) = &refusal else {
        panic!("the refusal is typed, not an Internal or a bare validation: {refusal:?}");
    };
    assert!(
        detail.contains(&format_rfc3339(cutover_at())),
        "the detail names the instant the cutover is at, which is the operand deciding whether \
         the unit is live at all: {detail}"
    );

    // The same refusal over the wire, on the route that raises it. The plane
    // assertions below then cover this attempt as well as the one above.
    let response = h
        .allowed_as(SUBMITTER)
        .send(request(
            "POST",
            &PLAN_RETIRE.replace("{planId}", &plan_id.get().to_string()),
            Some(serde_json::json!({ "dry_run": false })),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        problem_code(response).await,
        bss_pricing::domain::retirement::RETIRE_OVER_LIVE_CUTOVER
    );

    let conn = h.db.conn().expect("conn");

    // (a) The cutover's unit, exactly as the cutover left it.
    let units = approval_repo::list_page(&conn, &h.scope(), h.tenant, &[], None, 100)
        .await
        .expect("read the approval plane");
    let cutover_unit = units
        .iter()
        .find(|record| record.subject_ref == unit_ref)
        .unwrap_or_else(|| panic!("the cutover's unit is on record: {unit_ref}"));
    assert_eq!(
        cutover_unit.state,
        ApprovalState::Approved,
        "the refused retirement decided nothing on the cutover's unit"
    );

    // And it opened no unit of its own. `retirement_unit_ref` names the act, so a
    // unit under that subject is the thing a refusal before the submit must not
    // have written.
    let retirement_ref =
        bss_pricing::infra::approval::retirement_unit_ref(plan_id, seeded.revision);
    assert!(
        !units
            .iter()
            .any(|record| record.subject_ref == retirement_ref),
        "the guard runs before the retirement's own submit, so no unit stands under \
         {retirement_ref}: {units:?}"
    );

    // (b) The window plane, untouched. Not "nothing was cancelled" but "nothing
    //     moved at all": the refusal is before every writer.
    let rows = window_rows(&h).await;
    assert!(
        !rows.iter().any(|row| row.state == "cancelled"),
        "the refusal cancelled nothing: {rows:?}"
    );
    let born_at_the_cutover: Vec<&price_window::Model> = rows
        .iter()
        .filter(|row| row.effective_from == cutover_at())
        .collect();
    assert_eq!(
        born_at_the_cutover.len(),
        2,
        "the successor's and the copy's windows: {rows:?}"
    );
    assert!(
        born_at_the_cutover
            .iter()
            .all(|row| row.state == "scheduled" && row.effective_to.is_none()),
        "both still scheduled and open-ended, exactly as the composition wrote them: \
         {born_at_the_cutover:?}"
    );
    let shortened = rows
        .iter()
        .find(|row| row.window_id == receipt.shortened_window_id)
        .expect("the predecessor's window");
    assert_eq!(
        shortened.effective_to,
        Some(cutover_at()),
        "the predecessor still ends at the cutover, and now nothing has retired the plan out \
         from under the successor that takes over there"
    );

    // (c) The plan itself. This is the clause that separates a refusal from a
    //     half-applied unwind: the revision never flipped.
    assert!(
        !plan_lifecycle_states(&h, plan_id)
            .await
            .iter()
            .any(|state| state == "retired"),
        "the flip is downstream of the guard, so no revision of the plan reads retired"
    );

    // The positive control: a plan with no cutover still retires, in this same
    // world and through this same call.
    let (other_plan, _) = published_plan(&h).await;
    committed_retirement(&h, other_plan).await;
    assert!(
        plan_lifecycle_states(&h, other_plan)
            .await
            .iter()
            .any(|state| state == "retired"),
        "positive control: the guard refuses a live cutover and not every retirement"
    );
}

/// Every `lifecycle_state` the plan's revision rows carry.
async fn plan_lifecycle_states(h: &Harness, plan_id: PlanId) -> Vec<String> {
    let conn = h.db.conn().expect("conn");
    bss_pricing::infra::storage::entity::plan::Entity::find()
        .secure()
        .scope_with(&h.scope())
        .filter(
            Condition::all()
                .add(bss_pricing::infra::storage::entity::plan::Column::TenantId.eq(h.tenant))
                .add(bss_pricing::infra::storage::entity::plan::Column::PlanId.eq(plan_id.get())),
        )
        .all(&conn)
        .await
        .expect("read the plan rows")
        .into_iter()
        .map(|row| row.lifecycle_state)
        .collect()
}

// ---------------------------------------------------------------------------
// (4) What lands with the D-79 lane, on the day it lands.
// ---------------------------------------------------------------------------

/// Ordinary D-51 retirement opens D-05's trailing void by itself, the moment the
/// presence lane resolves — and yesterday's coverage guard does not close it,
/// because it protects a different key.
///
/// D-182 keeps `PresenceMap::fail_closed()` the built system's only case, so
/// today the condemned set is empty and the case above measures a retirement that
/// cancels nothing. This one asks the domain the question the lane will make
/// answerable, over the window plane a committed cutover leaves behind:
///
/// * the predecessor, on the `all_subscriptions` key, ending **at** the cutover;
/// * the successor, on that same key, `[cutover, …)` — a candidate, not yet active;
/// * the grandfathered copy, on the generation's key, `[cutover, …)` — also a
///   candidate.
///
/// With nobody in flight, `dispose_windows` condemns both candidates and
/// `strand_free_disposition` re-keeps only the **copy**: D-04's bound is a
/// question about a grandfathered generation's coverage, and the successor is not
/// on one. So the `all_subscriptions` key is left with a predecessor that stops at
/// the cutover and nothing after it — uncovered from the cutover instant onward,
/// which is exactly the trailing void D-05 exists to prevent and no gap check can
/// see.
///
/// That reframes what the unwind is owed for. Slice 11's AC list files it as
/// *"Slice 7's to build"*, a missing callee; it is also a **latent defect of this
/// surface**, armed and waiting on the same lane D-316's residual waits on. The
/// two arrive together and neither is closed by the lane landing.
///
/// The positive control is the copy's verdict: the guard does fire here, on the
/// key it is for, so a green on the successor's line is not the guard being inert.
#[test]
fn when_the_lane_lands_retirement_cancels_the_cutovers_successor_and_keeps_only_the_copy() {
    use bss_pricing::domain::retirement::{
        GenerationCoverage, KeptReason, PresenceMap, ScheduledWindow, WindowDisposition,
        dispose_windows, strand_free_disposition,
    };
    use bss_pricing::domain::window::{WindowInterval, WindowState};

    let successor_price = Uuid::from_u128(0x_5c_a1);
    let copy_price = Uuid::from_u128(0x_5c_a2);
    let successor_window = Uuid::from_u128(0x_5c_b1);
    let copy_window = Uuid::from_u128(0x_5c_b2);

    // Both post-cutover windows are candidates; the shortened predecessor is
    // active and therefore never in this set.
    let scheduled = [
        ScheduledWindow {
            window_id: successor_window,
            price_id: successor_price,
        },
        ScheduledWindow {
            window_id: copy_window,
            price_id: copy_price,
        },
    ];

    // The lane answered: nobody is bound to either row.
    let mut verdicts = dispose_windows(&scheduled, &PresenceMap::resolved(std::iter::empty()));
    assert!(
        verdicts.iter().all(WindowVerdictExt::cancelled),
        "before the guard runs, both are condemned: {verdicts:?}"
    );

    // The generation the copy sits on, with a horizon and a margin, so the bound
    // is computable and the guard can actually decide.
    let generation = GenerationCoverage {
        scope_key: rest_support::publishable_scope_key(
            PlanId::new(Uuid::from_u128(0x_5c_c5)),
            bss_pricing::domain::scope_key::PhaseId::new(Uuid::from_u128(0x_5c_c6)),
            "eu",
        )
        .to_generation(cutover_at())
        .expect("the generation key"),
        windows: vec![(
            copy_window,
            WindowInterval::new(cutover_at(), None, WindowState::Scheduled),
        )],
        grandfather_until: Some(utc_ymd_hms(2100, 8, 20, 0, 0, 0)),
        margin: Some(time::Duration::days(31)),
    };
    strand_free_disposition(&mut verdicts, &[generation], now());

    let successor = verdicts
        .iter()
        .find(|v| v.window_id == successor_window)
        .expect("the successor's verdict");
    assert_eq!(
        successor.disposition,
        WindowDisposition::Cancelled,
        "the successor's window is cancelled while the predecessor still ends at the cutover - \
         the `all_subscriptions` key is uncovered from the cutover onward, and this is D-05's \
         trailing void arriving from ordinary D-51 retirement with no unwind involved"
    );

    let copy = verdicts
        .iter()
        .find(|v| v.window_id == copy_window)
        .expect("the copy's verdict");
    assert_eq!(
        copy.disposition,
        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound),
        "positive control: the guard does fire, on the generation it is for. It is not inert \
         here - it simply has nothing to say about a key that carries no generation"
    );
}

/// `WindowVerdict::is_cancelled` under a name `all` can take.
trait WindowVerdictExt {
    fn cancelled(&self) -> bool;
}

impl WindowVerdictExt for bss_pricing::domain::retirement::WindowVerdict {
    fn cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

// ---------------------------------------------------------------------------
// (5) The clause that is paid, under another name.
// ---------------------------------------------------------------------------

/// D-05's always-material half is still paid under D-109 — and **the reason
/// D-327 gave for it stopped being true on 2026-08-16**.
///
/// D-05 registered retirement-with-a-live-cutover as always material; D-109 then
/// registered retirement **unconditionally**, and `retire_in` declares
/// `Trigger::PlanRetirement` on every path. D-327 clause (4) declared the D-05
/// clause paid on the ground that the two declarations produce **byte-identical
/// verdicts**: `MaterialityVerdict` was `Material { reason, tripped }`, so the
/// trigger's identity reached no column and no response, and this case asserted
/// the two evaluations equal.
///
/// The verdict now carries the trigger, so they are **not** equal. What survives
/// is the conclusion and not the argument: retirement is material either way, and
/// under exactly the same rule, so no publish that used to need a second principal
/// stops needing one and none newly does. What changed is that a producer for
/// `RetirementUnwindingACutover` would now be **observable** — the record would say
/// `retirementUnwindingACutover` where it says `planRetirement` — which removes
/// the very argument D-321 used to refuse building one for the bundle pair. That
/// is owed back to the register rather than decided here: it makes D-327's clause
/// (4) rest on the weaker claim it can still support, and it turns "a producer here
/// would change nothing observable" from a reason into a falsehood.
///
/// The trigger is still not spare: it is the vocabulary entry Slice 5's
/// `inst-mat-registered` enumerates, and D-321 clause (3)'s rule is that a correct
/// declaration the design set still owes is not dead code. It still has no
/// producer, which is D-327 clause (1)'s state and unchanged by this.
#[test]
fn a_retirement_over_a_live_cutover_is_material_under_the_trigger_it_already_declares() {
    let with_cutover = materiality::evaluate(
        &ChangeSet::of_act(Trigger::RetirementUnwindingACutover, std::iter::empty()),
        None,
        None,
    );
    let plain = materiality::evaluate(
        &ChangeSet::of_act(Trigger::PlanRetirement, std::iter::empty()),
        None,
        None,
    );

    // The half that carries D-05's clause: same answer, same rule, so the
    // publish's authorization requirement is identical under either declaration.
    assert!(with_cutover.is_material() && plain.is_material());
    assert_eq!(
        with_cutover.reason(),
        plain.reason(),
        "both are `alwaysMaterialTrigger`, so retirement needs a second principal \
         whichever trigger declares it, which is what makes D-05's clause paid"
    );

    // And the half D-327 clause (4) got wrong, now that there is somewhere for the
    // act to be recorded. This assertion is the inverse of the one it replaces.
    assert_ne!(
        with_cutover, plain,
        "the two declarations are distinguishable in the record as of 2026-08-16, so \
         `a producer here would change nothing observable` is no longer a reason for \
         anything"
    );
    assert_eq!(
        with_cutover.trigger(),
        Some(Trigger::RetirementUnwindingACutover)
    );
    assert_eq!(plain.trigger(), Some(Trigger::PlanRetirement));

    assert!(
        matches!(
            plain,
            MaterialityVerdict::Material {
                reason: MaterialityReason::AlwaysMaterialTrigger,
                ..
            }
        ),
        "positive control: the verdict `retire_in` actually declares is the material one \
         D-05 asked for: {plain:?}"
    );
}

/// C3-2: a plan whose current revision can never be superseded takes no successor
/// row either, and this door was one of the two that did not ask.
///
/// **Reachability first, because the finding turns on it.** `plan_repo::retire_revision`
/// flips only the `plan` row's `lifecycle_state`; it does not touch `pricing_price`. So
/// after a committed retirement the plan's price rows still read `published`,
/// `read_cutover_context`'s `on_key(key, Published)` still finds the predecessor, and
/// nothing further down asks: `commit_supersession_rows` runs `refuse_mispaired` /
/// `supersede_row` / `publish_rows` and none of the three reads the plan's lifecycle,
/// while `RepoError::NoSuccessorRevision` — the source of `PLAN_RETIRED_NO_SUCCESSOR`
/// — is raised only by the revision-opening path this act never takes. The cutover
/// therefore ran to completion on a retired plan: two drafts staged, a registry handle
/// taken, the predecessor superseded and two rows published onto a plan nobody may buy.
///
/// The refusal is **permanent**, which is why `supersede_in` and `PublishService::commit`
/// hoist it ahead of the registry request and why this door now does too: a handle
/// requested past it stands pending forever and trips
/// `pricing.catalogversion.commit_overdue` for a publish that can never happen.
#[tokio::test]
async fn a_cutover_onto_a_retired_plan_is_refused_before_a_handle_is_taken() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    // **The act must be on the approved arm, or the handle assertion below cannot
    // fail.** A first, unapproved call returns `SubmittedForApproval` at step 3b --
    // well above step 10's registry request -- so on that arm no handle is taken
    // whether the refusal stands or not, and asserting the count would assert a
    // number the fixture pins for an unrelated reason. Measured: with step 1a
    // deleted, the unapproved call still answers `SubmittedForApproval` and never
    // reaches the registry.
    //
    // The re-post of an approved unit is also the shape D-156 is about: the
    // retirement lands between the approval and the re-post, which is the one
    // ordering a reviewer cannot see coming.
    let opened = cut_over(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens on a live plan");
    let approval_id = match &opened {
        CutoverOutcome::SubmittedForApproval(pending) => pending.approval.approval_id,
        CutoverOutcome::Committed(_) => panic!("the first call must open a unit"),
    };
    // **The retirement is repo-level, and that is the point.** Step 1a is defence
    // behind a door that already stands: `RETIRE_OVER_LIVE_CUTOVER` refuses the
    // retirement of a plan carrying a cutover unit in *any* live state -- submitted
    // and approved alike, measured -- so no ordering of the two services reaches
    // this state. `plan_repo::retire_revision` flips the plan row the way a
    // migration, a repair script or a future caller of the repository would, which
    // is exactly the class step 1a exists for.
    retire_plan(&h, plan_id).await;
    approve(&h, approval_id).await;

    let request = request_of(&key, 12_000);
    let staged = request.successor_price_id;
    let handles_before = h.registry.requested().len();
    let refused = cut_over(&h, request, SUBMITTER)
        .await
        .expect_err("a retired plan takes no successor row");

    assert!(
        matches!(refused, DomainError::PlanRetiredNoSuccessor(_)),
        "the hoisted refusal must be what answered: {refused:?}"
    );
    // Ahead of everything: step 1a is the first thing past the context read, so
    // nothing was staged and no version was requested.
    assert!(
        !window_rows(&h)
            .await
            .iter()
            .any(|row| row.effective_from == cutover_at()),
        "no window was composed for an act that may not happen"
    );
    assert!(
        price_row(&h, staged).await.is_none(),
        "and no draft was written"
    );
    // The sharper half of this case's own name. A stranded handle leaves no row --
    // `RegistryDouble::requested`'s doc says so -- so a refusal below the registry
    // request leaves every assertion above green while
    // `pricing.catalogversion.commit_overdue` fires forever for a publish that can
    // never happen.
    assert_eq!(
        h.registry.requested().len(),
        handles_before,
        "the refusal is hoisted ahead of the registry request: no handle may be taken"
    );
}

/// Retire the plan's current revision **through the repository**, bypassing
/// `RetirementService` and its `RETIRE_OVER_LIVE_CUTOVER` door.
///
/// `sqlite_cutover_unit`'s helper of the same name, for the same reason: the
/// service refuses to retire a plan that carries a live cutover unit, so the state
/// step 1a guards is reachable only from a writer that is not that service.
async fn retire_plan(h: &Harness, plan_id: PlanId) {
    use bss_pricing::infra::storage::repo::plan_repo;
    let revision =
        plan_repo::load_current(&h.db.conn().expect("conn"), &h.scope(), h.tenant, plan_id)
            .await
            .expect("read the current revision")
            .expect("the plan has one")
            .revision;
    let scope = h.scope();
    let tenant = h.tenant;
    let (_, outcome) =
        h.db.db()
            .in_transaction::<_, bss_pricing::infra::storage::RepoError, _>(move |txn| {
                Box::pin(async move {
                    plan_repo::retire_revision(txn, &scope, tenant, plan_id, revision).await
                })
            })
            .await;
    outcome.expect("retire the seeded plan revision");
}

/// Is there a `pricing_price` row under this id at all?
async fn price_row(
    h: &Harness,
    price_id: Uuid,
) -> Option<bss_pricing::infra::storage::entity::price::Model> {
    use bss_pricing::infra::storage::entity::price;
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::SecureEntityExt;
    let conn = h.db.conn().expect("conn");
    price::Entity::find()
        .secure()
        .scope_with(&h.scope())
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .one(&conn)
        .await
        .expect("read the row")
}
