//! The retirement orchestrator (`inst-rt-cancel`, `inst-rt-event`,
//! `inst-re-governed`, D-109, D-128, D-182).
//!
//! `sqlite_cutover_unit`'s shape, because the two acts are the same shape of
//! thing: an always-material unit that a single principal may compose and may
//! not commit. What differs is what the act *is* — a cutover stages two rows and
//! moves three windows, a retirement stages nothing and, in this system, moves
//! nothing at all.
//!
//! That last clause is the reason this file exists rather than a route test
//! standing in for it. D-182 makes the D-79 presence lane absent, D-131's
//! fail-closed clause then reads every key as occupied, and retirement keeps
//! every scheduled window. The distance between "keeps every window because the
//! rule says so" and "cancels nothing because nobody wired the loop" is not
//! visible from a green suite — it is visible from a preview that says *why*
//! each window was kept, and from a cancellation loop that runs over an empty
//! list rather than not existing.

#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;
mod rest_support;

use std::sync::Arc;

use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::retirement::{KeptReason, WindowDisposition, WindowVerdict};
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::retirement::{RetirementOutcome, RetirementService};

use bss_pricing::domain::instant::utc_ymd_hms;
use rest_support::Harness;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use time::OffsetDateTime;
use toolkit_db::secure::SecureEntityExt;
use uuid::Uuid;

const SUBMITTER: Uuid = Uuid::from_u128(0x_5c_12);
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_5c_c1);

/// Inside `common`'s coverage window `[2099-08-04, 2099-09-01)`.
fn now() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 5, 0, 0, 0)
}

fn stamp_of(actor: Uuid) -> bss_pricing::domain::audit::AuditStamp {
    bss_pricing::domain::audit::AuditStamp {
        actor_principal_id: actor,
        recorded_at: now(),
        correlation_id: TEST_CORRELATION,
    }
}

/// The plan's current lifecycle token, read straight off the row.
///
/// Local rather than added to `rest_support`: that module is a shared append
/// point between two concurrent strands, and a helper only this file needs is a
/// merge conflict nobody is buying anything with.
async fn plan_state(h: &Harness, plan_id: PlanId) -> String {
    let conn = h.db.conn().expect("conn");
    let rows = bss_pricing::infra::storage::entity::plan::Entity::find()
        .secure()
        .scope_with(&h.scope())
        .filter(
            Condition::all()
                .add(bss_pricing::infra::storage::entity::plan::Column::TenantId.eq(h.tenant))
                .add(bss_pricing::infra::storage::entity::plan::Column::PlanId.eq(plan_id.get())),
        )
        .all(&conn)
        .await
        .expect("read the plan rows");
    let mut states: Vec<String> = rows.into_iter().map(|row| row.lifecycle_state).collect();
    states.sort();
    states.join(",")
}

/// The pending unit over this plan's retirement, if any.
async fn open_retirement_unit(
    h: &Harness,
    plan_id: PlanId,
) -> Option<bss_pricing::infra::storage::repo::approval_repo::ApprovalRecord> {
    let conn = h.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::approval_repo::find_pending_for_subject(
        &conn,
        h.tenant,
        &bss_pricing::infra::approval::retirement_unit_ref(plan_id, 0),
    )
    .await
    .expect("read the approval plane")
}

fn service(h: &Harness) -> RetirementService {
    RetirementService::new(h.db.clone(), Arc::clone(&h.registry) as Arc<_>)
}

/// A published plan with one published price row and its coverage window.
async fn published_plan(h: &Harness) -> PlanId {
    let plan_uuid = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(h, plan_uuid).await;
    h.publish(plan_uuid, seeded.revision).await;
    h.publish_price(plan_uuid, seeded.price_id).await;
    PlanId::new(plan_uuid)
}

async fn retire(h: &Harness, plan_id: PlanId) -> Result<RetirementOutcome, DomainError> {
    service(h)
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

fn pending(outcome: &RetirementOutcome) -> &bss_pricing::infra::retirement::RetirementPending {
    match outcome {
        RetirementOutcome::SubmittedForApproval(pending) => pending,
        RetirementOutcome::Retired(_) => {
            panic!("a retirement must never commit on one principal (D-109)")
        }
    }
}

/// Seed a migration aimed at `target`, and return its id.
///
/// `source_plan_id` is a plan nothing else in the test uses: what blocks the
/// retirement is being the migration's **target**, and pinning the source to a
/// stranger keeps the probe from passing for the wrong reason.
async fn schedule_migration_targeting(h: &Harness, target: PlanId) -> Uuid {
    let conn = h.db.conn().expect("conn");
    let migration_id = Uuid::now_v7();
    bss_pricing::infra::storage::repo::migration_repo::insert_or_load(
        &conn,
        &h.scope(),
        bss_pricing::infra::storage::repo::migration_repo::NewMigration {
            migration_id,
            tenant_id: h.tenant,
            source_plan_id: PlanId::new(Uuid::now_v7()),
            source_revision: 0,
            target_plan_id: target,
            effective_at: now() + time::Duration::days(90),
            announced_at: now(),
            scope: serde_json::json!({ "kind": "all" }),
            delta_report: serde_json::json!({
                "locked": [], "entitlement": [], "addon": [], "boundary": []
            }),
            created_by: SUBMITTER,
            created_at: now(),
        },
    )
    .await
    .expect("schedule the migration");
    migration_id
}

#[tokio::test]
async fn a_live_migration_aimed_at_the_plan_refuses_the_retirement() {
    // `RETIRE_TARGET_OF_MIGRATION` had a code, a `DomainError` variant, an arm in
    // the error ladder, a purpose-built index and a repository reader
    // (`migration_repo::live_targeting`) whose only callers were two tests.
    // Nothing constructed the variant, so retiring a plan that in-flight
    // subscribers are being moved *onto* was refused by nothing.
    //
    // Kept apart from `RETIRE_PLAN_REFERENCED` on purpose, as the error ladder
    // says: one remedy is to re-compose a referrer, this one is to cancel the
    // migration or wait for it.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;
    let migration_id = schedule_migration_targeting(&h, plan_id).await;

    let refusal = retire(&h, plan_id)
        .await
        .expect_err("the retirement refuses");

    match &refusal {
        DomainError::RetireTargetOfMigration(detail) => {
            assert!(
                detail.contains(&migration_id.to_string()),
                "the refusal must name the schedule the caller has to act on: {detail}"
            );
        }
        other => panic!("expected RETIRE_TARGET_OF_MIGRATION, got {other:?}"),
    }

    // And nothing moved: the refusal is a guard, not a half-applied act.
    assert_eq!(plan_state(&h, plan_id).await, "published");
    assert!(open_retirement_unit(&h, plan_id).await.is_none());
}

#[tokio::test]
async fn a_migration_that_is_no_longer_live_does_not_refuse_the_retirement() {
    // The positive control the refusal above is worthless without, and the
    // narrowing `live_targeting` already documents: a completed run moves nobody,
    // so it must not block. Without this, widening the reader to *every* migration
    // would leave the suite green.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;
    let migration_id = schedule_migration_targeting(&h, plan_id).await;

    let conn = h.db.conn().expect("conn");
    bss_pricing::infra::storage::repo::migration_repo::start(
        &conn,
        &h.scope(),
        h.tenant,
        migration_id,
        serde_json::json!({}),
        now(),
    )
    .await
    .expect("start");
    bss_pricing::infra::storage::repo::migration_repo::complete(
        &conn,
        &h.scope(),
        h.tenant,
        migration_id,
        serde_json::json!({}),
        now(),
    )
    .await
    .expect("complete");

    let outcome = retire(&h, plan_id).await.expect("the retirement composes");
    assert!(matches!(
        outcome,
        RetirementOutcome::SubmittedForApproval(_)
    ));
}

#[tokio::test]
async fn a_retirement_never_commits_on_one_principal() {
    // D-109: retirement is a **registered** always-material trigger, so no
    // threshold policy can make this arm commit. It is the act D-62 made
    // two-person for a single window, applied to every key of a plan at once,
    // and it is irreversible.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;

    let outcome = retire(&h, plan_id).await.expect("the retirement composes");
    let pending = pending(&outcome);

    assert_eq!(pending.preview.plan_id, plan_id);
    assert_eq!(pending.preview.revision, 0);

    // And the plan is still published: the composed arm writes no flip.
    let state = plan_state(&h, plan_id).await;
    assert_eq!(
        state, "published",
        "the unit is open, the plan has not moved"
    );
}

#[tokio::test]
async fn a_retry_finds_the_open_unit_rather_than_opening_a_second() {
    // `retirement_unit_ref` is deterministic precisely so this holds. A second
    // unit over one retirement would put two decisions in a reviewer's queue for
    // one act, and deciding either would leave the other permanently
    // undecidable.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;

    let first = retire(&h, plan_id).await.expect("the first");
    let second = retire(&h, plan_id).await.expect("the retry");

    assert_eq!(
        pending(&first).approval.approval_id,
        pending(&second).approval.approval_id,
        "the retry must resolve the unit the first attempt opened"
    );
}

#[tokio::test]
async fn every_scheduled_window_is_kept_and_the_preview_says_why() {
    // D-182 through D-131's fail-closed clause. The seeded plan carries one
    // scheduled coverage window; with no presence lane its key reads occupied,
    // so the window is kept - and the preview labels it `PresenceUnresolved`
    // rather than `InFlightSubscribers`, because "kept because nobody could be
    // asked" is not the same fact for the operator confirming.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;

    let preview = service(&h)
        .preview(&h.scope(), h.tenant, plan_id)
        .await
        .expect("the dry-run");

    assert!(
        preview.presence_unresolved,
        "the D-79 lane has no client in this system"
    );
    assert!(
        !preview.windows.is_empty(),
        "the seed must carry a scheduled window for this case to mean anything"
    );
    assert!(
        preview
            .windows
            .iter()
            .all(|w| w.disposition == WindowDisposition::Kept(KeptReason::PresenceUnresolved)),
        "{:?}",
        preview.windows
    );
    assert_eq!(
        preview.windows.iter().filter(|w| w.is_cancelled()).count(),
        0
    );
}

#[tokio::test]
async fn the_dry_run_writes_nothing() {
    // `inst-rt-api`'s first clause, and D-61's reviewability invariant depends on
    // it: the approver reads this screen **before** deciding, so a preview that
    // opened a unit would make reading it an act.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;

    service(&h)
        .preview(&h.scope(), h.tenant, plan_id)
        .await
        .expect("the dry-run");

    assert_eq!(plan_state(&h, plan_id).await, "published");
    assert!(
        open_retirement_unit(&h, plan_id).await.is_none(),
        "a dry-run opens no unit"
    );
}

#[tokio::test]
async fn a_plan_that_is_not_published_is_refused_with_its_own_state() {
    // Asked of the state machine, so the refusal names the state rather than
    // saying "something is in the way". An operator told their plan is already
    // retired stops; one told "forbidden" goes looking for a permission.
    let h = Harness::new().await;
    let plan_id = published_plan(&h).await;
    h.retire(plan_id.get(), 0).await;

    let refusal = service(&h)
        .preview(&h.scope(), h.tenant, plan_id)
        .await
        .expect_err("a retired plan does not retire again");
    let DomainError::LifecycleForbidden(detail) = refusal else {
        panic!("expected LifecycleForbidden, got {refusal:?}");
    };
    assert!(detail.contains("retired"), "{detail}");
}

// ---------------------------------------------------------------------------
// `inst-re-cancelflow`'s second clause — the event, which is half of "invoked"
// (this file's own module doc: "cancels nothing because nobody wired the loop").
// ---------------------------------------------------------------------------

/// Every event name enqueued for this tenant after `floor`, in `seq` order.
///
/// By **sequence and not by name**, `sqlite_cutover_unit`'s rule: a name filter
/// has to know what the fixture produces and silently drops the act's own event
/// the day the act starts producing one.
async fn events_since(h: &Harness, floor: i64) -> Vec<String> {
    use bss_pricing::infra::storage::entity::outbox;
    use sea_orm::Order;
    let conn = h.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&toolkit_db::secure::AccessScope::allow_all())
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

async fn outbox_floor(h: &Harness) -> i64 {
    use bss_pricing::infra::storage::entity::outbox;
    use sea_orm::Order;
    let conn = h.db.conn().expect("conn");
    outbox::Entity::find()
        .secure()
        .scope_with(&toolkit_db::secure::AccessScope::allow_all())
        .filter(Condition::all().add(outbox::Column::TenantId.eq(h.tenant)))
        .order_by(outbox::Column::Seq, Order::Desc)
        .one(&conn)
        .await
        .expect("read the outbox")
        .map_or(0, |row| row.seq)
}

/// The window's stored state token.
async fn window_state(h: &Harness, window_id: Uuid) -> String {
    use bss_pricing::infra::storage::entity::price_window;
    let conn = h.db.conn().expect("conn");
    price_window::Entity::find()
        .secure()
        .scope_with(&h.scope())
        .filter(
            Condition::all()
                .add(price_window::Column::TenantId.eq(h.tenant))
                .add(price_window::Column::WindowId.eq(window_id)),
        )
        .one(&conn)
        .await
        .expect("read the window")
        .expect("the seeded window is there")
        .state
}

/// Run the cancellation flow over `verdicts` in one transaction.
async fn cancel(h: &Harness, verdicts: Vec<WindowVerdict>) -> Vec<Uuid> {
    let scope = h.scope();
    let tenant = h.tenant;
    let (_, outcome) =
        h.db.db()
            .in_transaction::<Vec<Uuid>, DomainError, _>(move |txn| {
                Box::pin(async move {
                    bss_pricing::infra::retirement::cancel_windows_in(
                        txn,
                        &scope,
                        tenant,
                        &verdicts,
                        "retirement/fixture",
                        now(),
                        stamp_of(SUBMITTER),
                    )
                    .await
                })
            })
            .await;
    outcome.expect("the cancellation flow")
}

/// **A retirement-cancelled window emits `PriceWindowCancelled`.**
///
/// `inst-re-cancelflow`: Slice 7's cancellation flow is *invoked* — *"each
/// cancellation emits `PriceWindowCancelled` and drives its cache-eviction
/// path"* — and *"marking-invalid without the event is forbidden (consumers
/// would keep warm caches)"*. Until this case, `retire_in` moved the row through
/// `window_repo::transition` and enqueued nothing, under a comment asserting the
/// opposite. Every other window-mutating service in the crate pairs the flip
/// with its own enqueue; retirement was the one that did not.
///
/// **It is driven through the flow rather than through `retire_in`**, and that
/// is the point rather than a shortcut: D-182 keeps the presence map fail-closed,
/// so `retire_in` condemns nothing and the loop never runs — which is exactly
/// why the bug was green. A rule that fires only once a lane nobody has built
/// starts answering has to be armed directly, or the day the D-79 lane lands it
/// fires wrong.
#[tokio::test]
async fn a_retirement_cancelled_window_emits_its_event() {
    let h = Harness::new().await;
    let plan_uuid = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(&h, plan_uuid).await;
    h.publish(plan_uuid, seeded.revision).await;
    h.publish_price(plan_uuid, seeded.price_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let floor = outbox_floor(&h).await;
    let cancelled = cancel(
        &h,
        vec![WindowVerdict {
            window_id,
            price_id: seeded.price_id,
            disposition: WindowDisposition::Cancelled,
        }],
    )
    .await;

    assert_eq!(cancelled, vec![window_id], "the flow answers what it moved");
    assert_eq!(
        window_state(&h, window_id).await,
        "cancelled",
        "the row moved, which was never the missing half"
    );
    assert_eq!(
        events_since(&h, floor).await,
        vec!["PriceWindowCancelled".to_owned()],
        "the cancellation announces itself, or every consumer keeps a warm cache \
         over a window that no longer exists"
    );
}

/// **The control: a kept window is neither moved nor announced.**
///
/// Without it the case above passes against a flow that cancelled and announced
/// every window it was handed, which is the D-51 hazard inverted — a key with
/// in-flight subscribers losing its continuing coverage.
#[tokio::test]
async fn a_kept_window_is_left_alone_and_announces_nothing() {
    let h = Harness::new().await;
    let plan_uuid = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(&h, plan_uuid).await;
    h.publish(plan_uuid, seeded.revision).await;
    h.publish_price(plan_uuid, seeded.price_id).await;
    let window_id = common::coverage_window_id(seeded.price_id);

    let floor = outbox_floor(&h).await;
    let cancelled = cancel(
        &h,
        vec![WindowVerdict {
            window_id,
            price_id: seeded.price_id,
            disposition: WindowDisposition::Kept(KeptReason::InFlightSubscribers),
        }],
    )
    .await;

    assert!(cancelled.is_empty(), "a kept window is not a cancellation");
    assert_eq!(window_state(&h, window_id).await, "scheduled");
    assert!(
        events_since(&h, floor).await.is_empty(),
        "and nothing is announced about it"
    );
}

// ---------------------------------------------------------------------------
// D-04's `inst-co-bounds`, reached from retirement (D-316's residual).
//
// `infra::window::refuse_horizon_uncovered` binds a grandfathered generation's
// coverage across `[max(cohort, now), grandfatherUntil + the margin)` on the
// three acts `mutate_in` performs. Retirement's cancellations reach the window
// store through `window_repo::transition` and pass none of them, and the key a
// retirement cancels on is by construction one the presence lane reported
// **unoccupied** — which is exactly the generation D-04 has spoken for and D-51
// cannot see.
//
// These cases drive `compose_preview_with` against a **resolved** map, because
// the orchestrator can hand it only `fail_closed` and a rule that fires only once
// a lane nobody has built starts answering is a rule no suite driving the
// orchestrator can arm. That is `cancel_windows_in`'s posture, one function up.
// ---------------------------------------------------------------------------

/// The generation's cutover instant — its `cohort` axis, and the first instant it
/// can strand anybody.
///
/// `common`'s coverage start, so the fixture's generation is one whose window a
/// cutover would have composed **at** the cohort (`algo-cutover` step 3, D-204
/// clause (4)) rather than one already stranded before this file's `now()`.
fn generation_cohort() -> OffsetDateTime {
    utc_ymd_hms(2099, 8, 4, 0, 0, 0)
}

/// The horizon the fixture's generation is grandfathered until.
fn generation_horizon() -> OffsetDateTime {
    utc_ymd_hms(2099, 10, 1, 0, 0, 0)
}

/// `generation_horizon()` + W6's margin.
///
/// The seeded plan is `monthly` and W6 rounds a month **up** to its calendar
/// maximum — 31 days — because a margin rounded down leaves the tail of a bound
/// period uncovered, which is the hole D-04 exists to close. Spelled as the
/// arithmetic so the case says what the floor *is*.
fn generation_floor() -> OffsetDateTime {
    generation_horizon() + time::Duration::days(31)
}

/// A published plan carrying an ordinary row **and** a grandfathered generation
/// on its own cohort key, the generation's window running `[from, to)`.
///
/// Both rows carry a window, because the two are this suite's subject and its
/// control: the ordinary key has no horizon and must stay cancellable, the
/// generation has one and is what the bound is about.
async fn plan_with_a_generation(
    h: &Harness,
    from: OffsetDateTime,
    to: Option<OffsetDateTime>,
) -> (PlanId, Uuid, Uuid) {
    use bss_pricing::domain::scope_key::{Cohort, PriceEligibility};

    let plan_uuid = Uuid::now_v7();
    let seeded = rest_support::seed_publishable_plan(h, plan_uuid).await;
    let generation = rest_support::seed_price_keyed_with_horizon(
        h,
        plan_uuid,
        "us",
        PriceEligibility::ExistingGrandfathered,
        Cohort::Generation(generation_cohort()),
        Some(generation_horizon()),
    )
    .await;
    h.publish(plan_uuid, seeded.revision).await;
    h.publish_price(plan_uuid, seeded.price_id).await;
    h.publish_price(plan_uuid, generation.price_id).await;
    schedule_window(h, generation.price_id, from, to).await;
    (PlanId::new(plan_uuid), seeded.price_id, generation.price_id)
}

/// Put one window on a price row, straight through the store.
///
/// **Through `window_repo::schedule` and not the window service**, which is what
/// makes the already-stranded control below constructible at all: the service is
/// where `refuse_horizon_uncovered` lives, and it refuses exactly the interval
/// that control needs to seed. The store is also where a *retirement* meets the
/// window plane, so the fixture and the subject reach it by the same door.
async fn schedule_window(
    h: &Harness,
    price_id: Uuid,
    effective_from: OffsetDateTime,
    effective_to: Option<OffsetDateTime>,
) -> Uuid {
    use bss_pricing::infra::storage::repo::window_repo::{NewWindow, schedule};
    let conn = h.db.conn().expect("conn");
    let window_id = Uuid::now_v7();
    schedule(
        &conn,
        &h.scope(),
        NewWindow {
            window_id,
            tenant_id: h.tenant,
            price_id,
            effective_from,
            effective_to,
            reason_code: "fixtureGeneration".to_owned(),
        },
        stamp_of(SUBMITTER),
    )
    .await
    .expect("seed the generation's window");
    window_id
}

/// Compose the retirement judgement against a lane that answered **nobody is on
/// any of these rows** — the state D-182 keeps this system out of, and the one
/// the D-79 client will produce on its first green day.
async fn disposition_with_an_empty_lane(h: &Harness, plan_id: PlanId) -> Vec<WindowVerdict> {
    let conn = h.db.conn().expect("conn");
    bss_pricing::infra::retirement::compose_preview_with(
        &conn,
        &h.scope(),
        h.tenant,
        plan_id,
        bss_pricing::domain::retirement::PresenceMap::resolved([]),
        now(),
    )
    .await
    .expect("compose the retirement preview")
    .windows
}

/// **A grandfathered generation's sole scheduled window is kept, however empty
/// the presence lane says its key is.**
///
/// The defect D-316 recorded and did not build: the lane is asked per **price
/// id** and reports who is bound to a row *now*, while D-04's bound is about what
/// the generation must still cover — `[max(cohort, now), grandfatherUntil + the
/// margin)`. A generation reading unoccupied and holding the only window across
/// that span is precisely the case D-51's predicate waves through, and cancelling
/// it leaves the generation with no window at all: every bound period that
/// started before the horizon is unrateable from the cancellation onward, which
/// is D-04's stranding verbatim.
///
/// It is a **keep** and not a refusal. Refusing would make a plan carrying a
/// generation unretirable for as long as its horizon runs, with no remedy the
/// operator can reach; keeping costs retirement nothing it is for, since
/// not-sellable comes from the lifecycle flip and never from a window.
#[tokio::test]
async fn a_generations_sole_window_is_kept_against_an_empty_presence_lane() {
    let h = Harness::new().await;
    let (plan_id, _ordinary, generation_price) =
        plan_with_a_generation(&h, generation_cohort(), Some(generation_floor())).await;
    let generation_window = disposition_with_an_empty_lane(&h, plan_id)
        .await
        .into_iter()
        .find(|v| v.price_id == generation_price)
        .expect("the generation's window is in the preview");

    assert_eq!(
        generation_window.disposition,
        WindowDisposition::Kept(KeptReason::GrandfatheredCoverageBound),
        "cancelling the generation's only window strands what D-04 protects, and the \
         preview has to say so under its own reason rather than under D-51's"
    );
}

/// **The positive control: an ordinary key's window still cancels.**
///
/// Without it the case above is satisfied by a guard that kept every window it
/// was shown — which is what retirement already does under D-182's fail-closed
/// map, and which would make the D-79 lane's arrival change nothing at all. The
/// ordinary row of the *same plan* and the *same preview* carries no horizon, so
/// it has no bound to be kept by.
#[tokio::test]
async fn an_ordinary_keys_window_still_cancels_on_an_empty_lane() {
    let h = Harness::new().await;
    let (plan_id, ordinary_price, _generation) =
        plan_with_a_generation(&h, generation_cohort(), Some(generation_floor())).await;

    let ordinary = disposition_with_an_empty_lane(&h, plan_id)
        .await
        .into_iter()
        .find(|v| v.price_id == ordinary_price)
        .expect("the ordinary row's window is in the preview");

    assert_eq!(
        ordinary.disposition,
        WindowDisposition::Cancelled,
        "a key with no grandfathering horizon is D-51's ordinary case and cancels"
    );
}

/// **The second control: a generation whose span is *already* broken stays
/// cancellable.**
///
/// D-316 clause (3) accepts a void lying strictly in the past — no interval can
/// be authored backwards over it, and a rule anchored so as to refuse it would
/// freeze the key against every act that repairs its future. The retirement guard
/// inherits that reading and it is what the two walks buy: the condemned set is
/// re-kept only when the void opens **because of** the cancellation. Here the
/// generation's window opens ten days past its own floor, so the span is
/// uncovered before retirement touches anything and keeping the window would buy
/// the stranded population nothing while making the key unretirable forever.
///
/// A one-walk guard — "is the span uncovered afterwards" — passes the case above
/// and reddens here, which is the whole reason this control exists.
#[tokio::test]
async fn an_already_stranded_generation_does_not_become_unretirable() {
    let h = Harness::new().await;
    let (plan_id, _ordinary, generation_price) =
        plan_with_a_generation(&h, generation_floor() + time::Duration::days(10), None).await;

    let generation_window = disposition_with_an_empty_lane(&h, plan_id)
        .await
        .into_iter()
        .find(|v| v.price_id == generation_price)
        .expect("the generation's window is in the preview");

    assert_eq!(
        generation_window.disposition,
        WindowDisposition::Cancelled,
        "a generation already uncovered across its own span gains nothing from the \
         window being kept, and D-316 clause (3) is explicit that such a key must \
         stay mutable"
    );
}

/// **And the fail-closed map still keeps everything, which is the world as built.**
///
/// The guard runs after the presence judgement and only ever moves a verdict back
/// to `kept`, so D-182's posture is untouched: with no lane every key reads
/// occupied and every window is kept **under D-51's reason**, not under this one.
/// Without this case a guard that re-labelled every kept window would pass the
/// three above and silently rewrite what the confirm screen tells an operator on
/// every retirement this system can actually perform.
#[tokio::test]
async fn the_absent_lane_still_keeps_every_window_under_its_own_reason() {
    let h = Harness::new().await;
    let (plan_id, _ordinary, _generation) =
        plan_with_a_generation(&h, generation_cohort(), Some(generation_floor())).await;

    let preview = service(&h)
        .preview(&h.scope(), h.tenant, plan_id)
        .await
        .expect("the dry-run");

    assert!(
        !preview.windows.is_empty(),
        "the plan has windows to decide about"
    );
    for verdict in &preview.windows {
        assert_eq!(
            verdict.disposition,
            WindowDisposition::Kept(KeptReason::PresenceUnresolved),
            "D-182's absent lane is what keeps these, and the screen must keep saying so"
        );
    }
}
