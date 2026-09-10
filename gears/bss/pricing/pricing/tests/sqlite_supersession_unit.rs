//! The supersession **unit** — `inst-su-compose`'s composer and `inst-su-commit`'s
//! owner, driven end to end against a real database.
//!
//! # What this suite owns that `sqlite_supersession.rs` cannot
//!
//! That file's subject is the four writes: two planes, one transaction, one order.
//! It builds its `SupersessionCommit` by hand and never asks who is allowed to build
//! one. What is here is the half above it, and every property of it is about
//! **sequence** rather than about a store:
//!
//! * the successor draft is staged **before** the unit is opened, because the unit's
//!   content pin covers the plan's `draft` rows — so a pin taken first is a pin over a
//!   world without the row under review, and no reviewer can ever approve it. (This
//!   file first said the reason was that the door "voids every `submitted` unit of the
//!   plan". That is **false** for this unit — `void_pending_for_plan` filters
//!   `subject_kind = 'plan_revision'` and a supersession unit is a `price_unit` — and
//!   the false version survived here for one commit *after* the correction landed
//!   elsewhere, which is the stale-correction failure this file's own §5 warns about.)
//! * the *same act arriving twice* is answered with the same unit
//!   (`inst-su-api`'s idempotency per `(planId, scope key, changeover instant)`), and
//!   that lookup happens **before the evaluator is asked**, so a threshold raised
//!   between the two calls cannot make an act under review commit on one principal;
//! * every **permanent** refusal is answered before the `CatalogVersion` request
//!   (D-156), so a request no retry can ever complete strands no pending handle;
//! * the two events `inst-su-return` names are enqueued and the **third is not** —
//!   the predecessor's `PriceWindowExpired` belongs to the sweep at the changeover.
//!
//! None of those is visible from a unit test over a composed plan, and none of them
//! is visible from the store: each is a statement about what did **not** happen.
//!
//! # Every instant is a fixed date
//!
//! The changeover floors and `inst-ws-future-start` both compare against a clock, so
//! the fixtures sit inside `common`'s 2099 coverage window and the "now" every case
//! stamps with is a constant. A fixture computed from `OffsetDateTime::now_utc()` asserts something
//! different every day it runs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;
mod rest_support;

use std::sync::Arc;

use bss_pricing::domain::approval::{DecisionBy, WithdrawAuthority};
use bss_pricing::domain::audit::{AuditStamp, AuditSubjectKind};
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::events::CatalogEvent;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::materiality::{MaterialityReason, MaterialityVerdict};
use bss_pricing::domain::money::{MinorAmount, RateMinor};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::scope_key::{Cohort, PlanId, PriceEligibility, ScopeKey};
use bss_pricing::domain::window::WindowState;
use bss_pricing::infra::approval::{DecideRequest, RegionGrant};
use bss_pricing::infra::storage::entity::{audit_log, outbox, price, price_window};
use bss_pricing::infra::storage::repo::approval_repo;
use bss_pricing::infra::supersession::{
    SupersessionOutcome, SupersessionRequest, SupersessionService,
};
use time::OffsetDateTime;

use bss_pricing::domain::instant::format_rfc3339;
use rest_support::{Harness, Publishable};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

/// The submitter of every supersession here, and the independent principal who
/// approves its unit.
const SUBMITTER: Uuid = Uuid::from_u128(0x_5b_11);
const REVIEWER: Uuid = Uuid::from_u128(0x_5b_22);
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_5b_c0);

/// The suite's fixed reading of "now": inside `common`'s coverage window
/// `[2099-08-04, 2099-09-01)` and well before the changeover.
fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 5, 0, 0, 0)
}

/// The changeover: inside the predecessor's coverage and clear of **both** of
/// `inst-su-instant`'s floors.
fn changeover() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 20, 0, 0, 0)
}

fn stamp_of(actor: Uuid) -> AuditStamp {
    AuditStamp {
        actor_principal_id: actor,
        recorded_at: now(),
        correlation_id: TEST_CORRELATION,
    }
}

/// A published plan with one published price row on the `eu` key, and the coverage
/// window that row's key holds.
///
/// Published for real through the repository doors, for `rest_support`'s stated
/// reason: a suite whose subject is the supersession unit must not depend on the
/// publish route's four preconditions.
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

/// The successor's content: the seeded row repriced, and nothing structural moved.
fn successor_content(amount: i64) -> PriceContent {
    let mut content = rest_support::publishable_row();
    content.row.amount_minor = Some(MinorAmount::new(amount).expect("non-negative"));
    content
}

fn request_of(key: &ScopeKey, amount: i64) -> SupersessionRequest {
    SupersessionRequest {
        key: key.clone(),
        changeover: changeover(),
        successor: successor_content(amount),
        successor_price_id: Uuid::now_v7(),
        successor_window_id: Uuid::now_v7(),
        reason_code: "repricing".to_owned(),
    }
}

fn service(h: &Harness) -> SupersessionService {
    SupersessionService::new(h.db.clone(), Arc::clone(&h.registry) as Arc<_>)
}

async fn supersede(
    h: &Harness,
    request: SupersessionRequest,
    actor: Uuid,
) -> Result<SupersessionOutcome, DomainError> {
    service(h)
        .supersede(
            &rest_support::security_context(actor, h.tenant),
            &h.scope(),
            h.tenant,
            request,
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(actor),
        )
        .await
}

/// Approve the unit the first call opened, with an independent principal.
async fn approve(h: &Harness, approval_id: Uuid) {
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

// ---------------------------------------------------------------------------
// Readbacks. Every one reads with `allow_all`, so what is asserted is what
// landed rather than what the caller was allowed to see.
// ---------------------------------------------------------------------------

async fn rows_on_key(h: &Harness, key: &ScopeKey) -> Vec<price::Model> {
    let conn = h.db.conn().expect("conn");
    price::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(price::Column::TenantId.eq(h.tenant)))
        .order_by(price::Column::PriceId, Order::Asc)
        .all(&conn)
        .await
        .expect("read the price rows")
        .into_iter()
        // More than `(currency, region)`: this suite seeds a usage key that shares
        // both with the recurring one, and a coarser filter counted the other key's
        // rows as this one's.
        //
        // Less than the canonical key. `plan_id`, `phase`, `cohort`,
        // `price_overlay`, `meter` and `dimension_key` go uncompared, and what
        // makes that sound is not a property of the key but of this file's
        // fixtures: the only pair of keys a harness here holds at once is the
        // recurring one and the usage one, and `charge_kind` separates them. A
        // second usage line, or a second overlay, on an otherwise identical key
        // would be the first fixture this helper could not tell apart.
        .filter(|row| {
            row.currency == key.currency().as_str()
                && row.region == key.region().as_str()
                && row.charge_kind == key.charge_kind().as_str()
                && row.price_eligibility == key.price_eligibility().as_str()
        })
        .collect()
}

async fn state_of(h: &Harness, price_id: Uuid) -> String {
    let conn = h.db.conn().expect("conn");
    price::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(price::Column::PriceId.eq(price_id)))
        .one(&conn)
        .await
        .expect("read the row")
        .expect("the row is there")
        .lifecycle_state
}

async fn window_of(h: &Harness, window_id: Uuid) -> Option<price_window::Model> {
    let conn = h.db.conn().expect("conn");
    price_window::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(price_window::Column::WindowId.eq(window_id)))
        .one(&conn)
        .await
        .expect("read the window")
}

/// The outbox's high-water mark, taken after the fixture and before the act.
///
/// See [`event_names_since`] for why the fixture is excluded by **when** here and
/// by **name** in `sqlite_publish_commit`.
async fn outbox_floor(h: &Harness) -> i64 {
    let conn = h.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(outbox::Column::TenantId.eq(h.tenant)))
        .order_by(outbox::Column::Seq, Order::Desc)
        .one(&conn)
        .await
        .expect("read the outbox")
        .map_or(0, |row| row.seq)
}

/// Every event name the act wrote after `floor`, in `seq` order.
///
/// **The fixture is excluded by when, not by what.** This helper filtered
/// `PriceCreated` out by name until 2026-08-06, on the reasoning that the seed
/// authors a price row and authoring is where S3 §17.5 puts that event. The
/// reasoning is right about the seed and wrong about this file: the act under test
/// stages its **successor draft** through
/// `insert_successor_draft_on` -> `write_prepared` -> `record_price_mutation(Create)`,
/// which enqueues `PriceCreated`. So a name filter deleted an
/// event the compose really produced, and `"nothing was announced"` was asserted
/// over an announcement.
///
/// A sequence floor cannot make that mistake: it says "what happened after the
/// fixture" without needing to know which names the fixture uses. The name filter
/// stays correct in `sqlite_publish_commit`, where the paths under test author no
/// rows — the generalisation from that file to this one is what failed, and it
/// failed because the second file's act was not re-read.
async fn event_names_since(h: &Harness, floor: i64) -> Vec<String> {
    let conn = h.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(h.tenant))
                .add(outbox::Column::Seq.gt(floor)),
        )
        .order_by(outbox::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the outbox")
        .into_iter()
        .map(|row| row.event_name)
        .collect()
}

async fn event_payload(h: &Harness, event: CatalogEvent) -> serde_json::Value {
    let conn = h.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(outbox::Column::TenantId.eq(h.tenant))
                .add(outbox::Column::EventName.eq(event.as_str())),
        )
        .one(&conn)
        .await
        .expect("read the outbox")
        .unwrap_or_else(|| panic!("no {} event was enqueued", event.as_str()))
        .payload
}

/// Every audit entry of `subject_kind = price_unit`, in `seq` order.
async fn price_unit_audit(h: &Harness) -> Vec<audit_log::Model> {
    let conn = h.db.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(
            Condition::all()
                .add(audit_log::Column::TenantId.eq(h.tenant))
                .add(audit_log::Column::SubjectKind.eq(AuditSubjectKind::PriceUnit.as_str())),
        )
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the audit log")
}

fn pending(
    outcome: &SupersessionOutcome,
) -> &bss_pricing::infra::supersession::SupersessionPending {
    match outcome {
        SupersessionOutcome::SubmittedForApproval(pending) => pending,
        SupersessionOutcome::Committed(_) => {
            panic!("the act committed where a second principal was required")
        }
    }
}

fn receipt(
    outcome: &SupersessionOutcome,
) -> &bss_pricing::infra::supersession::SupersessionReceipt {
    match outcome {
        SupersessionOutcome::Committed(receipt) => receipt,
        SupersessionOutcome::SubmittedForApproval(_) => {
            panic!("the act opened a unit where it was entitled to commit")
        }
    }
}

// ---------------------------------------------------------------------------
// The controlled arm: what it writes, and everything it must not.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_material_supersession_stages_its_successor_and_opens_a_unit() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let floor = outbox_floor(&h).await;

    let request = request_of(&key, 12_000);
    let staged_id = request.successor_price_id;
    let outcome = supersede(&h, request, SUBMITTER)
        .await
        .expect("a material supersession opens its unit");
    let pending = pending(&outcome);

    // The draft `inst-su-compose` clause (a) authors, on the predecessor's own key,
    // carrying the link D-127 requires and the door stamps.
    assert_eq!(pending.successor_price_id, staged_id);
    let rows = rows_on_key(&h, &key).await;
    assert_eq!(rows.len(), 2, "the predecessor and its staged successor");
    let staged = rows
        .iter()
        .find(|row| row.price_id == staged_id)
        .expect("the successor is on the key");
    assert_eq!(staged.lifecycle_state, LifecycleState::Draft.as_str());
    assert_eq!(staged.supersedes_price_id, Some(seeded.price_id));

    // The unit, under the token S5 §6 declares and the subject that names the act.
    assert_eq!(
        pending.approval.subject_kind,
        AuditSubjectKind::PriceUnit,
        "S5 section 6 declares no `supersession` member and this gear mints none"
    );
    assert!(
        pending.approval.subject_ref.contains("/supersession/"),
        "the subject names the act: {}",
        pending.approval.subject_ref
    );
    assert!(
        pending
            .approval
            .subject_ref
            .contains(&format_rfc3339(changeover())),
        "and the changeover, to the millisecond: {}",
        pending.approval.subject_ref
    );
    assert_eq!(
        approval_repo::held_keys_of(
            &h.db.conn().expect("conn"),
            &h.scope(),
            h.tenant,
            pending.approval.approval_id
        )
        .await
        .expect("read the register"),
        vec![key.to_string()],
        "one key, the one being repriced"
    );

    // And **nothing else moved**: the four things a committed unit touches.
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str(),
        "the predecessor is still the key's current row"
    );
    let coverage = window_of(&h, common::coverage_window_id(seeded.price_id))
        .await
        .expect("the predecessor's window is there");
    assert_eq!(
        coverage.effective_to,
        Some(common::coverage_to()),
        "its end has not moved to the changeover"
    );
    assert!(
        rest_support::pending_version_refs(&h).await.is_empty(),
        "no CatalogVersion was requested for an act that did not commit"
    );
    // **One event, and it is the staged draft's own.** The compose announces
    // nothing about the act — no `PriceUpdated`, no window event, nothing a
    // consumer could read as a repricing having happened — but it does author a
    // row, and S3 §17.5 puts `PriceCreated` on row authoring. Asserting the exact
    // list rather than emptiness is what makes both halves of that statable; this
    // case claimed "nothing was announced" until 2026-08-06, over an outbox reader
    // that deleted the announcement by name.
    assert_eq!(
        event_names_since(&h, floor).await,
        vec![CatalogEvent::PriceCreated.as_str().to_owned()],
        "the compose authors the successor draft and announces exactly that"
    );
    assert!(
        price_unit_audit(&h)
            .await
            .iter()
            .all(|entry| entry.action != "publish"),
        "the unit's own record is written by the commit, not by the compose"
    );
}

#[tokio::test]
async fn the_same_act_arriving_twice_is_answered_with_the_same_unit() {
    // `inst-su-api`: idempotent per `(planId, scope key, changeover instant)`. The
    // second call must find the unit rather than refuse it, and the lookup runs before
    // the evaluator — so a threshold policy raised between the two calls cannot turn an
    // act under review into one that commits on a single principal.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let first = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the first call opens the unit");
    // A second request of the same act, with its own freshly minted ids — which is
    // what a retrying client actually sends.
    let second = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the retry is answered, not refused");

    assert_eq!(
        pending(&second).approval.approval_id,
        pending(&first).approval.approval_id,
        "one act, one unit"
    );
    assert_eq!(
        pending(&second).successor_price_id,
        pending(&first).successor_price_id,
        "and the staged row's id, never the id the retry minted"
    );
    assert_eq!(
        rest_support::approval_rows(&h)
            .await
            .iter()
            .filter(|record| record.subject_ref.contains("/supersession/"))
            .count(),
        1,
        "no second unit was opened"
    );
    assert_eq!(
        approval_repo::find_pending_for_subject(
            &h.db.conn().expect("conn"),
            h.tenant,
            &pending(&first).approval.subject_ref
        )
        .await
        .expect("read the store")
        .map(|record| record.approval_id),
        Some(pending(&first).approval.approval_id),
        "and the unit the first call opened is still submitted rather than voided"
    );
    assert_eq!(
        rows_on_key(&h, &key).await.len(),
        2,
        "and the successor was staged once"
    );
}

#[tokio::test]
async fn a_request_that_disagrees_with_the_staged_successor_is_refused() {
    // The hazard the reuse of a staged draft creates: the content pin covers the
    // plan shape, which carries the **staged** row — so an approved unit matches
    // whatever the retry's body says, and a commit that trusted the body would
    // publish content nobody reviewed. The refusal is the store's own
    // `DUPLICATE_SCOPE_KEY`, with a sentence the store cannot write.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let refused = supersede(&h, request_of(&key, 13_000), SUBMITTER)
        .await
        .expect_err("a different price is a different act on a key already staged");
    let DomainError::DuplicateScopeKey(message) = refused else {
        panic!("got: {refused:?}");
    };
    assert!(
        message.contains("different content"),
        "the sentence has to say what the store's cannot: {message}"
    );
    assert_eq!(
        rows_on_key(&h, &key).await.len(),
        2,
        "and no second draft was authored"
    );
}

// ---------------------------------------------------------------------------
// The commit arm.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approved_unit_commits_all_four_writes_and_announces_two_events() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);
    let floor = outbox_floor(&h).await;

    let opened = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let after_compose = outbox_floor(&h).await;
    let unit = pending(&opened).approval.approval_id;
    let successor_id = pending(&opened).successor_price_id;
    approve(&h, unit).await;

    let committed = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the call after an independent approve commits");
    let receipt = receipt(&committed);
    assert_eq!(receipt.successor_price_id, successor_id);
    assert_eq!(receipt.predecessor_price_id, seeded.price_id);
    assert_eq!(receipt.changeover, changeover());

    // The row plane.
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Superseded.as_str()
    );
    assert_eq!(
        state_of(&h, successor_id).await,
        LifecycleState::Published.as_str()
    );
    // The window plane, and the property the whole unit exists for: one instant,
    // two writes, no gap.
    let shortened = window_of(&h, receipt.shortened_window_id)
        .await
        .expect("the predecessor's window");
    let scheduled = window_of(&h, receipt.successor_window_id)
        .await
        .expect("the successor's window");
    assert_eq!(shortened.effective_to, Some(changeover()));
    assert_eq!(scheduled.effective_from, changeover());
    assert_eq!(scheduled.effective_to, None, "open-ended by decision");
    assert_eq!(scheduled.state, WindowState::Scheduled.as_str());
    assert_eq!(shortened.effective_to, Some(scheduled.effective_from));

    // The pending ref, carrying the plan subject the projector re-freezes.
    let refs = rest_support::pending_version_refs(&h).await;
    let mine = refs
        .iter()
        .find(|row| row.pending_ref == receipt.pending_version_ref)
        .expect("the handle this commit requested was recorded");
    assert_eq!(mine.subject_ref, plan_id.to_string());
    assert_eq!(
        mine.subject_kind,
        bss_pricing::domain::read_model::SubjectRef::Plan(plan_id.get())
            .kind()
            .as_str(),
        "the plan subject D-99/D-128 project, not a subject of this act's own"
    );
    // Against the **seed's** revision, not the receipt's. Both `mine.subject_revision`
    // and `receipt.revision` are `context.revision`, so comparing them can see the two
    // copies diverge and cannot see the revision replaced (review, 2026-08-06).
    assert_eq!(
        u64::try_from(mine.subject_revision.expect("a plan subject carries one"))
            .expect("non-negative"),
        seeded.revision
    );
    assert_eq!(receipt.revision, seeded.revision);

    // `inst-su-return`'s two events, and **only** those two.
    assert_eq!(
        event_names_since(&h, after_compose).await,
        vec![
            CatalogEvent::PriceUpdated.as_str().to_owned(),
            CatalogEvent::PriceWindowScheduled.as_str().to_owned(),
        ],
        "the predecessor's PriceWindowExpired is the sweep's at the changeover, not this \
         transaction's"
    );
    // And the **act's** whole trail, over the two calls it takes: the compose's
    // `PriceCreated` for the draft it staged, then the commit's two. Both floors are
    // asserted because they answer different questions — `inst-su-return` is a claim
    // about the commit, while a consumer subscribing to this plan sees the union —
    // and because a reader of the two-event assertion alone would conclude the act
    // announces nothing before its approve.
    assert_eq!(
        event_names_since(&h, floor).await,
        vec![
            CatalogEvent::PriceCreated.as_str().to_owned(),
            CatalogEvent::PriceUpdated.as_str().to_owned(),
            CatalogEvent::PriceWindowScheduled.as_str().to_owned(),
        ],
        "the compose announced the successor's creation before the approve"
    );
    let updated = event_payload(&h, CatalogEvent::PriceUpdated).await;
    assert_eq!(updated["priceId"], serde_json::json!(successor_id));
    assert_eq!(
        updated["supersedesPriceId"],
        serde_json::json!(seeded.price_id)
    );
    assert_eq!(updated["scopeKey"], serde_json::json!(key.to_string()));
    assert_eq!(
        updated["pendingVersionRef"],
        serde_json::json!(receipt.pending_version_ref)
    );

    // The record of who did it, at the act's level, naming the unit that authorized
    // it. One entry for the act rather than one per row moved.
    let trail: Vec<(String, Option<Uuid>)> = price_unit_audit(&h)
        .await
        .into_iter()
        .filter(|entry| entry.subject_ref == pending(&opened).approval.subject_ref)
        .map(|entry| (entry.action, entry.approval_ref))
        .collect();
    // The whole trail of one act under **one** subject string: the unit opening, the
    // independent approve, and the commit. That the three share a subject is what makes
    // them joinable at all — the approval record stands under the same string, so
    // `approval_ref` on the last entry closes the loop rather than pointing sideways.
    assert_eq!(
        trail,
        vec![
            ("submit".to_owned(), Some(unit)),
            ("approve".to_owned(), Some(unit)),
            ("publish".to_owned(), Some(unit)),
        ],
        "one act, one subject, three records"
    );
}

/// The supersession records the `effectiveTo` it overwrites — the **second** half of
/// D-327's defect, which nothing had found.
///
/// # Why this case exists at all
///
/// D-327 found the missing before-image on the grandfathering cutover, where it had a
/// symptom: D-05 has a *decided* unwind clause naming *"its recorded pre-cutover
/// value"*, so the absent operand was the reason a clause could not be built. This
/// path had the identical defect and no symptom — `commit_supersession` step 3 reaches
/// `window_repo::adjust_effective_to` exactly as `commit_cutover` does, `unit_state`
/// named two price ids and the key and no window and no instant, and no decision asks
/// a supersession to be undone. So "what did this key's coverage end at before the
/// reprice" had no answer here either, and nothing was going to notice.
///
/// It is worth the same fix rather than a lesser one because the destruction is the
/// same: `compose_windows` picks the covering window by `WindowInterval::covers`,
/// satisfied by an absent end *and* by any end strictly after the changeover, so after
/// the shorten the two are one row. And it is fixed through the **same renderer** —
/// `infra::window::shortened_window_state_value` — so a reader who learns the field
/// from one record can read the other. The parallel case is
/// `sqlite_cutover_unwind::a_cutover_records_the_effective_to_it_overwrites`.
///
/// Read through `audit_repo::for_subject`, which is what makes the record an operand
/// rather than a field: the subject is rebuilt from the unit's own string, not
/// remembered from a chain id and a `seq`.
///
/// The positive control is the `after_state` half — the same field, same record,
/// carrying the changeover — so a green is the pre-image being preserved and not the
/// pair being rendered twice from the post-write row.
#[tokio::test]
async fn a_supersession_records_the_effective_to_it_overwrites() {
    use bss_pricing::infra::storage::repo::audit_repo;

    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    // The fixture's coverage window is finite, and this is the value under test.
    // Read from the constants the window was **scheduled** from, and not from the
    // row under assertion: an expectation rebuilt from the row it checks cannot
    // see that row replaced, while these are the seed's own inputs and move only
    // when the seed does.
    let coverage_from = common::coverage_from();
    let coverage_to = common::coverage_to();

    let opened = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let unit = pending(&opened).approval.approval_id;
    let subject_ref = pending(&opened).approval.subject_ref.clone();
    approve(&h, unit).await;
    let committed = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the approved act commits");
    let receipt = receipt(&committed);

    // Positive control on the write: the shorten happened, and the truth side no
    // longer holds the old end.
    let shortened = window_of(&h, receipt.shortened_window_id)
        .await
        .expect("the predecessor's window is still there");
    assert_eq!(
        shortened.effective_to,
        Some(changeover()),
        "the act moved the predecessor's end to the changeover"
    );

    let records = audit_repo::for_subject(
        &h.db.conn().expect("conn"),
        &h.scope(),
        h.tenant,
        AuditSubjectKind::PriceUnit,
        &subject_ref,
    )
    .await
    .expect("read the act's records");
    let publish = records
        .iter()
        .find(|row| row.action == "publish")
        .unwrap_or_else(|| {
            panic!(
                "the commit's own record stands under the act's subject; the subject holds {:?}",
                records
                    .iter()
                    .map(|row| row.action.clone())
                    .collect::<Vec<_>>()
            )
        });

    let before_state = publish
        .before_state
        .as_ref()
        .expect("the commit's record carries a before-image");
    assert_eq!(
        before_state["shortenedWindow"],
        serde_json::json!({
            "scopeKey": key.to_string(),
            "effectiveFrom": format_rfc3339(coverage_from),
            "effectiveTo": format_rfc3339(coverage_to),
            "state": WindowState::Scheduled.as_str(),
        }),
        "the interval the shorten found, keyed by the two things a reader holding only the act \
         can match on — the record's subject names no window: {before_state}"
    );

    let after_state = publish.after_state.as_ref().expect("and an after-image");
    assert_eq!(
        after_state["shortenedWindow"],
        serde_json::json!({
            "scopeKey": key.to_string(),
            "effectiveFrom": format_rfc3339(coverage_from),
            "effectiveTo": format_rfc3339(changeover()),
            "state": WindowState::Scheduled.as_str(),
        }),
        "and the pair differs in exactly the one field the act moved: {after_state}"
    );
}

#[tokio::test]
async fn an_immaterial_supersession_commits_on_one_principal() {
    // `inst-su-commit`: "materiality = the standard per-currency price-delta
    // evaluation". A configured bar the delta does not reach makes the act
    // auto-publishable, and the whole unit runs on one call. Without this case every
    // other case here would pass against a service that refuses everything.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let committed = supersede(&h, request_of(&key, 10_000), SUBMITTER)
        .await
        .expect("a sub-threshold reprice commits directly");
    let receipt = receipt(&committed);
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Superseded.as_str()
    );
    assert_eq!(
        state_of(&h, receipt.successor_price_id).await,
        LifecycleState::Published.as_str()
    );
    // No unit at all, and the audit record says so rather than naming one.
    assert!(
        rest_support::approval_rows(&h)
            .await
            .iter()
            .all(|record| !record.subject_ref.contains("/supersession/")),
        "an immaterial act opens no unit"
    );
    let entry = price_unit_audit(&h)
        .await
        .into_iter()
        .find(|entry| entry.subject_ref.contains("/supersession/"))
        .expect("the act is still recorded");
    assert_eq!(
        entry.approval_ref, None,
        "and the record does not name an approval that does not exist"
    );
}

// ---------------------------------------------------------------------------
// The refusals, and the ordering that keeps each of them ahead of the registry.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_commit_floor_is_answered_before_any_catalog_version_is_requested() {
    // `inst-su-instant`'s commit bound is **permanent** for this request: the remedy
    // is a recomposition against a fresh instant, which is a different act with a
    // different subject and therefore a different registry handle. A refusal after
    // the request would leave the first handle pending forever (D-156, and the class
    // `PublishService::commit` hoists `refuse_unpublishable_predecessor` for).
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    // Inside `MAX_BATCHING_DELAY` of the stamped instant, and strictly future — so
    // the submit floor passes and only the commit floor can refuse it.
    let mut request = request_of(&key, 10_000);
    request.changeover = now() + time::Duration::minutes(2);
    let refused = supersede(&h, request, SUBMITTER)
        .await
        .expect_err("two minutes is inside the batching delay");
    assert!(
        matches!(refused, DomainError::SupersessionInstantPassed(_)),
        "got: {refused:?}"
    );
    assert!(
        rest_support::pending_version_refs(&h).await.is_empty(),
        "and no handle was requested for an act that can never complete"
    );
    assert_eq!(
        rows_on_key(&h, &key).await.len(),
        1,
        "and nothing was staged"
    );
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str()
    );
}

#[tokio::test]
async fn an_existing_grandfathered_generation_cannot_be_superseded() {
    // Foundation §4.3, and the refusal is the door's own — asked here one step
    // earlier for the reason above, through the one function both callers share.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let base = key_of(plan_id, &seeded);
    let retained = ScopeKey::new(
        plan_id,
        base.currency().clone(),
        base.region().clone(),
        seeded.phase,
        PriceEligibility::ExistingGrandfathered,
        base.charge_kind(),
        Cohort::Generation(utc_ymd_hms(2099, 1, 1, 0, 0, 0)),
    )
    .expect("existing_grandfathered carries a generation");

    let refused = supersede(&h, request_of(&retained, 10_000), SUBMITTER)
        .await
        .expect_err("a retained generation is immutable in price");
    // The class refusal is step **0** of the orchestrator and precedes even the read,
    // which is what this case pins: the key carries no published row at all, so a
    // `NotFound` here would mean the class question was asked too late to be the
    // permanent refusal D-156 needs ahead of the registry.
    let DomainError::LifecycleForbidden(message) = refused else {
        panic!("got: {refused:?}");
    };
    assert!(message.contains("existing_grandfathered"), "got: {message}");
    assert!(rest_support::pending_version_refs(&h).await.is_empty());
}

#[tokio::test]
async fn a_key_with_no_current_row_is_absent_rather_than_refused() {
    // The row-plane half of `inst-su-compose`'s presupposition of current coverage.
    // `NotFound` on the key rather than a conflict, because the remedy the design
    // names is a plain publish plus a window schedule — a different act.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let unpriced = rest_support::publishable_scope_key(plan_id, seeded.phase, "us");

    let refused = supersede(&h, request_of(&unpriced, 10_000), SUBMITTER)
        .await
        .expect_err("there is nothing on that key to supersede");
    assert!(
        matches!(refused, DomainError::NotFound { .. }),
        "got: {refused:?}"
    );
}

#[tokio::test]
async fn another_tenants_key_is_not_there_to_supersede() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let refused = service(&h)
        .supersede(
            &rest_support::security_context(SUBMITTER, h.other),
            &h.other_scope(),
            h.other,
            request_of(&key, 12_000),
            bss_pricing::api::rest::windows::verdict_json,
            stamp_of(SUBMITTER),
        )
        .await
        .expect_err("another tenant's plan is not visible");
    assert!(
        matches!(refused, DomainError::NotFound { .. }),
        "got: {refused:?}"
    );
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str(),
        "and nothing of mine moved"
    );
}

#[tokio::test]
async fn a_dormant_key_fails_compose_rather_than_committing_half_a_unit() {
    // `inst-su-compose`: a key with no scheduled or active window covering the
    // changeover presupposes nothing to hand over from. A changeover past the
    // seeded window's end is exactly that.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let mut request = request_of(&key, 10_000);
    request.changeover = common::coverage_to() + time::Duration::days(30);
    let refused = supersede(&h, request, SUBMITTER)
        .await
        .expect_err("coverage has already ended at that instant");
    let DomainError::SupersessionKeyDormant(message) = refused else {
        panic!("got: {refused:?}");
    };
    assert!(message.contains("dormant"), "got: {message}");
    assert_eq!(
        rows_on_key(&h, &key).await.len(),
        1,
        "nothing was staged for an act that cannot be composed"
    );
}

/// The verdict a pending unit reports is the evaluator's, carried rather than
/// re-minted — `infra::window::pending_approval`'s lesson one plane over.
#[tokio::test]
async fn the_pending_answer_carries_the_evaluators_own_reason() {
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let outcome = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    // **The evaluator's own reason, not merely "material"** — the first version of this
    // case asserted `Material { .. }`, which every sibling produces and which a
    // hard-coded constant in place of the carried verdict would also satisfy (review,
    // 2026-08-06). With no policy configured the answer is `inst-mat-failsafe`'s, and
    // naming it is what makes the value a carried one.
    assert_eq!(
        pending(&outcome)
            .verdict
            .as_ref()
            .expect("a freshly opened unit carries the evaluation that opened it")
            .reason(),
        Some(MaterialityReason::NoConfiguredThreshold)
    );
    assert_eq!(
        pending(&outcome).shortened_window_id,
        common::coverage_window_id(seeded.price_id),
        "and it names the window the composition would shorten"
    );
}

#[tokio::test]
async fn the_units_pin_covers_the_successor_it_stages() {
    // **The order's real ground, and it is not the one first written down.** The
    // successor draft is one of the plan's `draft` rows, so it is inside the shape
    // `submit_supersession_on` digests. A unit opened *before* the staging pins a world
    // without the row under review, and then no reviewer can ever approve it: the
    // re-derivation at decision time carries the draft the pin does not.
    //
    // A probe measured that, and it also measured that the reason recorded first — "the
    // door voids every submitted unit of the plan" — is false for this unit:
    // `void_pending_for_plan` filters `subject_kind = 'plan_revision'` and a supersession
    // unit is a `price_unit`. So this case, not that argument, is what holds the order.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let outcome = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let detail = h
        .governance
        .approvals
        .find(
            &h.scope(),
            h.tenant,
            pending(&outcome).approval.approval_id,
            now(),
        )
        .await
        .expect("read the unit back")
        .expect("it is there");
    assert!(
        detail.content_matches_pin,
        "a unit whose pin does not cover its own successor can be opened and never approved"
    );
}

#[tokio::test]
async fn the_stored_verdict_names_the_row_that_will_publish() {
    // The one field of the evaluator's input that has to be the **staged** row's and not
    // the id the request minted: `MaterialityVerdict::tripped_row` carries a `price_id`,
    // the verdict is stored on the unit as jsonb, and it is read years later. A retry
    // mints a fresh id, so a verdict built from the request would name a row that never
    // existed.
    //
    // This case exists because a removal proof came back **green**: dropping the
    // staged-id preference changed nothing, since every other case here reaches a verdict
    // with no tripped row at all (`noConfiguredThreshold`, `autoPublishable`). A bar the
    // delta *reaches* is what makes the field observable.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 100)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let first = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("9900 -> 12000 reaches a bar of 100");
    let staged_id = pending(&first).successor_price_id;
    let MaterialityVerdict::Material {
        tripped: Some(tripped),
        ..
    } = pending(&first)
        .verdict
        .as_ref()
        .expect("the opening call evaluates")
    else {
        panic!("got: {:?}", pending(&first).verdict);
    };
    assert_eq!(tripped.price_id, staged_id);

    // And the retry, whose request carries a **different** minted id, is a replay: it
    // evaluates nothing and carries no verdict of its own. The authority is the one the
    // unit **stored**, so that is what this half reads — which is also the assertion that
    // the two documents cannot disagree, since there is now only one.
    let second = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the retry is answered");
    assert!(
        pending(&second).verdict.is_none(),
        "a replay re-mints no verdict"
    );
    assert_eq!(
        pending(&second).approval.materiality["tripped"]["price_id"],
        serde_json::json!(staged_id),
        "the stored verdict names the row that is going to publish, never the id this \
         request minted and threw away"
    );
}

// ---------------------------------------------------------------------------
// The successor is judged and compared **as the store will hold it** — the two
// Criticals of the 2026-08-06 review, and both had one root cause.
// ---------------------------------------------------------------------------

/// A **usage** key of the seeded plan, and a published `graduated` row on it.
///
/// Every other fixture here is `recurring`, which is exactly why the two defects below
/// were invisible: `api::rest::prices::content_of` fills `charge_kind` with a
/// `Recurring` placeholder — `PriceContentView` has no such field — and the door rewrites
/// it from the key. On a recurring key the placeholder happens to be right.
async fn usage_key_with_published_row(
    h: &Harness,
    plan_id: PlanId,
    seeded: &Publishable,
) -> ScopeKey {
    let key = ScopeKey::new(
        plan_id,
        bss_pricing::domain::money::CurrencyCode::new("EUR").expect("three letters"),
        bss_pricing::domain::scope_key::Region::new("eu").expect("non-blank"),
        seeded.phase,
        PriceEligibility::NewSubscriptionsOnly,
        bss_pricing::domain::scope_key::ChargeKind::Usage,
        Cohort::None,
    )
    .expect("new_subscriptions_only pairs with cohort none");

    let price_id = Uuid::now_v7();
    // **The key the door files it under, not the one handed in** (D-196 clause 3).
    // `graduated_usage` prices the `api_calls` line, and the line is the key's
    // ninth and tenth axes now — derived from the content, because that is the
    // only half of an authoring request that can carry it. A fixture that kept
    // the line-less key would look the row up on a key no row is on, which is
    // what every caller of this helper would then be measuring.
    let authored = h
        .state
        .prices
        .create_draft(
            &h.scope(),
            h.tenant,
            bss_pricing::infra::storage::repo::NewPriceDraft {
                price_id,
                scope_key: key.clone(),
                content: graduated_usage(500),
                created_by: SUBMITTER,
                created_at_utc: now(),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the usage row");
    let key = authored.scope_key;
    let conn = h.db.conn().expect("conn");
    common::schedule_coverage_window(&conn, &h.scope(), h.tenant, price_id, stamp_of(SUBMITTER))
        .await;
    h.publish_price(plan_id.get(), price_id).await;
    key
}

/// A `graduated` usage row the S3 rule set passes: a meter, a quantization, a reset
/// window and a band set whose top band is open.
///
/// Modelled on `domain::rules_tests::graduated_usage`, which is what `price_row_rules`
/// is exercised against — so a row this suite calls legal is a row that set calls legal.
fn graduated_usage(unit_price: i64) -> PriceContent {
    use bss_pricing::domain::price_row::{
        BillingGranularity, ModelKind, PriceRow, TierAggregationWindow, TierBand,
    };
    let mut row = PriceRow::new(
        bss_pricing::domain::scope_key::ChargeKind::Usage,
        Some(ModelKind::Graduated),
    );
    row.meter = Some("api_calls".to_owned());
    row.billing_granularity = Some(BillingGranularity::WholeUnit);
    row.tier_aggregation_window = Some(TierAggregationWindow::CalendarMonth);
    row.bands = vec![TierBand::open(
        0,
        // Stated in whole minor units and scaled to the stored rate scale
        // (D-311), so this fixture prices what it always priced. Through
        // `from_minor_units` rather than a `* 1_000_000_000` literal, so the scale
        // keeps the single place `RATE_SUB_DECIMALS` claims for it (Z5-11).
        RateMinor::from_minor_units(unit_price).expect("non-negative"),
    )];
    PriceContent {
        row,
        tax_inclusive: false,
        tax_category_ref: None,
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn usage_request(key: &ScopeKey, amount: i64) -> SupersessionRequest {
    SupersessionRequest {
        key: key.clone(),
        changeover: changeover(),
        successor: graduated_usage(amount),
        successor_price_id: Uuid::now_v7(),
        successor_window_id: Uuid::now_v7(),
        reason_code: "repricing".to_owned(),
    }
}

#[tokio::test]
async fn a_usage_successor_that_moves_a_unit_field_is_refused_by_the_guard() {
    // **The Critical.** `SupersessionUnitGuard` gates on `successor.is_usage()`, and the
    // successor it was handed came off the wire with `charge_kind = Recurring` — the
    // placeholder `content_of` fills because `PriceContentView` has no such field. So on
    // a usage key the D-82/D-98/D-122/D-127/D-129 guard returned without evaluating, and
    // a successor could move `meter` under a continued tier counter: an hours-denominated
    // `Q` accumulated against one meter, re-read off another, mid-window. The guard was
    // live, correct, and unreachable through the one surface that has it.
    //
    // Every fixture in this file was `recurring`, which is why the suite was green.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = usage_key_with_published_row(&h, plan_id, &seeded).await;

    let mut request = usage_request(&key, 600);
    request.successor.row.meter = Some("api_bytes".to_owned());

    let refused = supersede(&h, request, SUBMITTER)
        .await
        .expect_err("the tier counter continues across a supersession");
    let DomainError::ValidationFailed(report) = refused else {
        panic!("got: {refused:?}");
    };
    let codes: Vec<&str> = report
        .violations
        .iter()
        .map(|violation| violation.code.as_str())
        .collect();
    assert!(
        codes.contains(&"SUPERSESSION_UNIT_MISMATCH"),
        "the unit guard must be what answered: {codes:?}"
    );
    assert_eq!(
        rows_on_key(&h, &key).await.len(),
        1,
        "and nothing was staged for a successor the guard refuses"
    );
}

#[tokio::test]
async fn a_usage_key_commits_after_its_approve_rather_than_refusing_itself_forever() {
    // **The second Critical, from the same root cause.** `refuse_divergent_successor`
    // compares the request against the **stored** successor, and the door rewrites
    // `charge_kind` from the key and sorts the bands. Un-normalized, the two sides
    // differed in `charge_kind` on every non-recurring key — so a legitimately approved
    // unit was refused `DUPLICATE_SCOPE_KEY` on its committing call, permanently, with a
    // remedy sentence ("re-send that content") no caller could act on: the wire cannot
    // spell `usage`.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = usage_key_with_published_row(&h, plan_id, &seeded).await;

    let opened = supersede(&h, usage_request(&key, 700), SUBMITTER)
        .await
        .expect("the unit opens on a usage key");
    approve(&h, pending(&opened).approval.approval_id).await;

    let committed = supersede(&h, usage_request(&key, 700), SUBMITTER)
        .await
        .expect("and the approved call commits rather than meeting its own guard");
    assert_eq!(
        state_of(&h, receipt(&committed).successor_price_id).await,
        LifecycleState::Published.as_str()
    );
}

#[tokio::test]
async fn a_successor_the_row_rules_refuse_never_reaches_the_published_plane() {
    // `inst-su-compose` clause (a): "the successor **draft** row on the same canonical
    // scope key (**S3 rules apply** — incl. the D-82/D-98 unit guard)". Only the guard
    // was running. `price_row_rules` has one other caller, `run_publish_rules`, and a
    // supersession successor never reaches it — D-195's exclusion rule drops a draft on
    // an occupied key from the plan-revision unit's set. So the row that publishes
    // through this unit was judged by **no** row-local rule: a `graduated` successor with
    // an empty band set had nothing to refuse it, no CHECK requires bands for a tiered
    // kind, and with zero band rows no band trigger fires.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = usage_key_with_published_row(&h, plan_id, &seeded).await;

    let mut request = usage_request(&key, 600);
    request.successor.row.bands.clear();
    let refused = supersede(&h, request, SUBMITTER)
        .await
        .expect_err("a tiered row with no bands prices nothing");
    // **The code by name, matching the sibling case above.** `SupersessionUnitGuard`
    // raises `ValidationFailed` too, so `matches!(.., ValidationFailed(_))` alone is
    // satisfied by any future rule on this path refusing for an unrelated reason —
    // while `price_row_rules` silently stops running, which is the exact regression
    // this case was written to close.
    let DomainError::ValidationFailed(report) = refused else {
        panic!("got: {refused:?}");
    };
    let codes: Vec<&str> = report
        .violations
        .iter()
        .map(|violation| violation.code.as_str())
        .collect();
    assert_eq!(
        codes,
        vec!["TIER_BANDS_GAP"],
        "the band geometry rule is what answered -- a ladder with no band covers no quantity, \
         so the gap starts at zero: {codes:?}"
    );
    assert!(
        !codes.contains(&"SUPERSESSION_UNIT_MISMATCH"),
        "and the unit guard is not what refused it: the request's unit matches its \
         predecessor's, so this case says nothing about `price_row_rules` if it did: {codes:?}"
    );
    assert_eq!(rows_on_key(&h, &key).await.len(), 1);
}

// ---------------------------------------------------------------------------
// The two-person control's own arms — the Highs.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_threshold_raised_after_the_submit_does_not_let_the_act_commit_alone() {
    // **A two-person bypass.** Materiality used to be evaluated first and the unit looked
    // up only on the material branch. A threshold policy raised between the submit and
    // the commit therefore made the act auto-publishable at the second call, and it
    // committed **on one principal** — leaving the unit that had been opened over it
    // `submitted` forever, asking a reviewer to decide an act that had already happened.
    // `evaluate`'s own contract is that materiality is decided *once, at submit*.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let opened = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("no policy configured, so the fail-safe opens a unit");
    let unit = pending(&opened).approval.approval_id;

    // A bar the delta cannot reach, approved by two principals as a governance act of
    // its own — it says nothing about this key.
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;

    let replayed = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the act is still under review");
    assert_eq!(
        pending(&replayed).approval.approval_id,
        unit,
        "a unit under review holds the act whatever a later policy says about it"
    );
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str(),
        "and one principal did not commit it"
    );
    assert!(
        pending(&replayed).verdict.is_none(),
        "a replay evaluates nothing, so it carries no second materiality document"
    );
}

#[tokio::test]
async fn an_immaterial_supersession_is_refused_while_another_unit_holds_the_key() {
    // `inst-co-single-pending` on the **committing** arm, which it did not have. The
    // window plane asks this unconditionally and before anything is touched; this path
    // asked it only inside `submit_supersession_on`, i.e. on the controlled arm — so an
    // immaterial supersession committed straight through a key another unit was
    // reviewing, moving the very interval set that reviewer had been shown.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    // Somebody else's unit holds this key. Opened through `submit_window_mutation_on`
    // directly rather than by driving a mutation: every act that would open one here is
    // refused first by a rule of its own — a cancel of the key's only window is
    // `WINDOW_TRAILING_VOID`, and a lengthening under the configured threshold is
    // auto-publishable and opens nothing. What this case is about is the supersession's
    // behaviour **under** a held key, not how the holder came to exist, and
    // `sqlite_window_service.rs` seeds holders the same way.
    let conn = h.db.conn().expect("conn");
    let holder = bss_pricing::infra::approval::ApprovalService::submit_window_mutation_on(
        &conn,
        &h.scope(),
        h.tenant,
        common::coverage_window_id(seeded.price_id),
        seeded.price_id,
        Uuid::now_v7(),
        serde_json::json!({ "reason": "alwaysMaterialTrigger" }),
        stamp_of(REVIEWER),
        // A well-formed subject: `subject_aggregate` parses `<plan_id>/<rest>` and a
        // corrupt one is refused before the register is written. The shape is
        // `infra::window::window_unit_ref`'s.
        &format!(
            "{plan_id}/{}/adjust/0/open/open",
            common::coverage_window_id(seeded.price_id)
        ),
    )
    .await
    .expect("the window unit opens and registers the key");
    assert_eq!(
        approval_repo::held_keys_of(&conn, &h.scope(), h.tenant, holder.approval_id)
            .await
            .expect("read the register"),
        vec![key.to_string()]
    );

    let refused = supersede(&h, request_of(&key, 10_000), SUBMITTER)
        .await
        .expect_err("a key under review is not a key to reprice through");
    assert!(
        matches!(refused, DomainError::PendingChangeUnitExists(_)),
        "got: {refused:?}"
    );
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str()
    );
}

#[tokio::test]
async fn a_grandfathering_horizon_is_refused_from_the_request_alone() {
    // A **fourth** permanent refusal that used to live inside the staging door, i.e.
    // after the registry request: `check_grandfather_horizon` refuses a horizon on every
    // class but `existing_grandfathered`, and step 0 has already refused that class
    // outright — so no value of this field can ever publish through a supersession, and
    // the refusal belongs where it needs no world at all.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let mut request = request_of(&key, 10_000);
    request.successor.grandfather_until = Some(changeover());
    let refused = supersede(&h, request, SUBMITTER)
        .await
        .expect_err("a supersession cannot author a horizon");
    assert!(
        matches!(refused, DomainError::GrandfatherUntilForbidden(_)),
        "got: {refused:?}"
    );
    assert!(
        rest_support::pending_version_refs(&h).await.is_empty(),
        "and no handle was requested for an act no retry can complete"
    );
    assert_eq!(rows_on_key(&h, &key).await.len(), 1);
}

#[tokio::test]
async fn the_submit_floor_is_the_lenient_one_and_the_commit_floor_is_not() {
    // The positive twin the floor case lacked. A changeover two minutes out is **legal at
    // submit** — `inst-su-instant`'s whole point is that a proposal is not held to the
    // batching delay, since it is going to be perfectly legal by the time a second person
    // approves it — and illegal at commit. Without this case, collapsing step 2's
    // `Submit` into `Commit` left the suite green.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let mut request = request_of(&key, 12_000);
    request.changeover = now() + time::Duration::minutes(2);
    let opened = supersede(&h, request, SUBMITTER)
        .await
        .expect("two minutes out is a legal proposal");
    assert!(
        pending(&opened)
            .approval
            .subject_ref
            .contains("/supersession/"),
        "the unit opened over it"
    );
}

#[tokio::test]
async fn a_replay_after_the_commit_is_refused_rather_than_replayed() {
    // The divergence `api::rest::supersessions` reports, given an executable statement.
    // After the commit the key's current row is the successor and its own window begins
    // exactly at the changeover, so `compose_windows`' begins-at-the-changeover arm
    // answers: shortening a window to its own start leaves nothing for a successor to
    // take over from. Legible and safe — nothing is written — but it is not the replay
    // S5's idempotency column reads as, which is why the divergence is reported.
    let h = Harness::new().await;
    rest_support::approve_threshold_policy(&h, &[("EUR", 1_000_000)]).await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    supersede(&h, request_of(&key, 10_000), SUBMITTER)
        .await
        .expect("the first call commits");
    let refused = supersede(&h, request_of(&key, 10_000), SUBMITTER)
        .await
        .expect_err("the act has already happened");
    let DomainError::SupersessionWouldEmptyWindow(message) = refused else {
        panic!("got: {refused:?}");
    };
    assert!(
        message.contains("the changeover itself"),
        "the successor's own window now begins there: {message}"
    );
}

#[tokio::test]
async fn a_rejected_unit_leaves_the_act_re_submittable_and_nothing_committed() {
    // The fourth outcome of the controlled arm, which nothing pinned: `submitted` is not
    // the only state a unit leaves. A reject is terminal **for the record** — §4's machine
    // — and the register row follows its parent out of `submitted`, so the key frees and
    // the next identical request opens a **fresh** unit rather than being refused or
    // committing. That is this gear's general rule for every unit kind; the case exists
    // because "the reviewer said no" and "the act may be re-asked" are easy to conflate.
    let h = Harness::new().await;
    let (plan_id, seeded) = published_plan(&h).await;
    let key = key_of(plan_id, &seeded);

    let opened = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("the unit opens");
    let first = pending(&opened).approval.approval_id;
    h.governance
        .approvals
        .decide(
            &h.scope(),
            h.tenant,
            DecideRequest {
                approval_id: first,
                decision: DecisionBy::Reject(REVIEWER),
                reason: Some("not this quarter".to_owned()),
                approver_regions: RegionGrant::Untransported,
                stamp: stamp_of(REVIEWER),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the reviewer refuses it");

    let reopened = supersede(&h, request_of(&key, 12_000), SUBMITTER)
        .await
        .expect("a refused act may be asked again");
    assert_ne!(
        pending(&reopened).approval.approval_id,
        first,
        "a fresh unit, because a decided record is immutable"
    );
    assert_eq!(
        state_of(&h, seeded.price_id).await,
        LifecycleState::Published.as_str(),
        "and the rejection committed nothing"
    );
}
