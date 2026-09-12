//! The approval workflow against a real database: the two-person rule, the
//! approver's scope, the mandatory reason, the TOCTOU void and the pin's
//! re-verification.
//!
//! # Every refusal here first puts the world in the state where the guard it
//! names is what answers
//!
//! Five checks share one decision path, and four of them can fire on a request
//! built carelessly. So the suite opens with
//! [`an_independent_reviewer_approves_the_content_they_were_shown`] — the world
//! in which every refusal below is a fact about the rule it names rather than
//! about a service that refuses everything — and each refusal moves exactly one
//! thing away from it.
//!
//! # The void is asserted per **surface**, because that is what it depends on
//!
//! `inst-ap-pin`'s correctness is a property of a *set* of mutating surfaces,
//! not of one mechanism: a surface that forgets to void is a reviewer approving
//! content that moved, and nothing about the guard itself would look wrong. So
//! there is one test per surface that must void, and one for the surface that
//! must **not** — `create_plan`, which cannot touch a subject pinned before the
//! plan existed. The full enumeration with its reasoning is in
//! `infra::approval::void_pending_units_of`'s doc.
//!
//! One of those surfaces is not on the void's rail and was missing from both the
//! enumeration and this file until 2026-08-04: `open_revision` appends its own
//! audit record, so it inherits nothing, and
//! [`opening_the_successor_revision_voids_the_unit_the_publish_orphaned`] is its
//! test. Its staging is the only one here that reaches a plan without passing
//! the rail at all, which is what lets it show a residue nothing else closes.
//! The publish commit itself closes its own since 2026-08-04
//! (`tests/rest_publish.rs`); this one is the backstop's test, and its staging
//! says so.
//!
//! # How the content mismatch is staged, and why not by mutating the subject
//!
//! The obvious staging — submit, mutate the plan, approve — **cannot reach the
//! mismatch**, and that is the guard working: the mutation voids the record
//! first, so the approve is answered `APPROVAL_NOT_PENDING`. What
//! `APPROVAL_CONTENT_MISMATCH` guards is the residue: a subject that moved
//! without passing the void rail at all.
//!
//! That residue is staged by opening a record whose pin does not match the
//! subject, with **nothing else different** — no row touched, no
//! `row_version` moved anywhere in the plan. This is the Phase-2 G7 lesson
//! applied: staging two revisions at different versions would let a plain
//! compare-and-swap answer 409 whether or not the pin is bound to anything, and
//! the test would prove nothing. Here every version coincides, so the pin is the
//! only thing that can produce the refusal.
//!
//! The other half of the evidence is elsewhere and is named so a reader can
//! follow it: that the *recomputed* digest tracks the content is
//! `domain::approval::content_pin`'s
//! `every_hashed_field_moves_the_pin`, and that an unmoved subject recomputes to
//! its pin is the positive control at the top of this file.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use bss_pricing::domain::approval::{ApprovalState, DecisionBy, WithdrawAuthority};
use bss_pricing::domain::audit::{AuditAction, AuditStamp, AuditSubjectKind};
use bss_pricing::domain::concurrency::RowVersion;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::lifecycle::LifecycleState;
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan::PlanShapePatch;
use bss_pricing::domain::plan_shape::{
    AddonRule, BillingCycle, DescriptorSet, Frequency, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::approval::{
    ApprovalService, DecideRequest, RegionGrant, TOCTOU_VOID_REASON,
};
use bss_pricing::infra::storage::entity::{audit_log, plan};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::approval_repo::{self, ApprovalRecord, NewApproval};
use bss_pricing::infra::storage::repo::plan_repo::NewPlanDraft;
use bss_pricing::infra::storage::repo::price_repo::NewPriceDraft;
use bss_pricing::infra::storage::repo::{PlanRepo, PlanShapeRepo, PriceRepo};
use time::OffsetDateTime;

use bss_pricing::domain::instant::format_rfc3339;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use sea_orm_migration::MigratorTrait;
use serde_json::json;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const SUBMITTER: Uuid = Uuid::from_u128(0x5b_01);
const APPROVER: Uuid = Uuid::from_u128(0xab_01);
/// `inst-as-void`'s **other** withdrawer: neither the submitter nor the
/// approver, and therefore the only one whose identity no column and no other
/// field of the record can be mistaken for.
const CATALOG_ADMIN: Uuid = Uuid::from_u128(0xca_01);

fn plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
}

fn other_plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a5))
}

fn terminal_phase() -> PhaseId {
    PhaseId::new(Uuid::from_u128(0xfa_5e))
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

fn stamp() -> AuditStamp {
    AuditStamp {
        actor_principal_id: SUBMITTER,
        recorded_at: at(11),
        correlation_id: TEST_CORRELATION,
    }
}

/// The stamp a decision is taken under: who acted, when, and the request's
/// correlation.
fn stamp_of(actor: uuid::Uuid, when: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

struct Harness {
    plans: PlanRepo,
    shapes: PlanShapeRepo,
    prices: PriceRepo,
    approvals: ApprovalService,
    scope: AccessScope,
    provider: DBProvider<DbError>,
}

async fn harness() -> Harness {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    let provider = DBProvider::<DbError>::new(db);
    Harness {
        plans: PlanRepo::new(provider.clone()),
        shapes: PlanShapeRepo::new(provider.clone()),
        prices: PriceRepo::new(provider.clone()),
        approvals: ApprovalService::new(provider.clone()),
        scope: AccessScope::for_tenant(TENANT),
        provider,
    }
}

fn new_plan_draft(id: PlanId) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id: id,
        tenant_id: TENANT,
        created_by: SUBMITTER,
        created_at_utc: at(10),
        sku_id: Some(Uuid::from_u128(0x5_c1)),
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: Some(Frequency::Monthly),
        plan_tier_override: false,
        purchase_min_qty: None,
        purchase_max_qty: None,
        invoice_grouping_key: None,
        available_from: None,
        available_to: None,
        cloned_from: None,
        correlation_id: TEST_CORRELATION,
    }
}

fn flat_row() -> PriceContent {
    let mut row = PriceRow::new(ChargeKind::Recurring, Some(ModelKind::Flat));
    row.amount_minor = Some(MinorAmount::new(9_900).expect("a non-negative amount"));
    PriceContent {
        row,
        tax_inclusive: false,
        // Authored rather than absent: the content pin these cases carry through
        // submit and commit frames the category's *value*, and a fixture holding
        // `None` exercises only its presence — the same hole
        // `content_pin_tests::maximal_record` closed.
        tax_category_ref: Some("standard".to_owned()),
        billing_timing: Some("advance".to_owned()),
        proration_contract: None,
        rounding_policy_ref: Some("half_up".to_owned()),
        grandfather_until: None,
        supersedes_price_id: None,
    }
}

fn scope_key(market: &str) -> ScopeKey {
    ScopeKey::new(
        plan_id(),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new(market).expect("a non-blank region"),
        terminal_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none")
}

/// One plan with a phase chain, a descriptor set and one `eu` price row, plus
/// the revision and the current row version of the plan revision.
struct Seeded {
    revision: u64,
    revision_version: RowVersion,
    price_id: Uuid,
    price_version: RowVersion,
}

async fn seed(h: &Harness) -> Seeded {
    let created = h
        .plans
        .create_draft(&h.scope, new_plan_draft(plan_id()))
        .await
        .expect("create the draft");
    let after_phases = h
        .shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan_id(),
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: terminal_phase(),
                kind: PhaseKind::Evergreen,
                display_name: None,
                ordinal: 0,
                converts_to_phase_id: None,
                phase_duration_days: None,
                display_trial_days: None,
            }],
            stamp(),
        )
        .await
        .expect("attach the phase chain");
    let after_descriptors = h
        .shapes
        .set_descriptor_set(
            &h.scope,
            TENANT,
            plan_id(),
            created.revision,
            after_phases.row_version,
            DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4000".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("attach the descriptor set");

    let price_id = Uuid::from_u128(0xb_0001);
    let row = h
        .prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id,
                scope_key: scope_key("eu"),
                content: flat_row(),
                created_by: SUBMITTER,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author the price row");

    Seeded {
        revision: created.revision,
        revision_version: after_descriptors.row_version,
        price_id,
        price_version: row.row_version,
    }
}

/// Open a pending unit over the seeded plan, pinning what is there now.
async fn submit(h: &Harness, approval_id: Uuid) -> ApprovalRecord {
    h.approvals
        .submit(
            &h.scope,
            TENANT,
            plan_id(),
            approval_id,
            json!({ "reason": "noConfiguredThreshold" }),
            stamp_of(SUBMITTER, at(12)),
        )
        .await
        .expect("open the pending unit")
}

fn approve_as(approval_id: Uuid, approver: Uuid, regions: &[&str]) -> DecideRequest {
    DecideRequest {
        approval_id,
        decision: DecisionBy::Approve(approver),
        reason: None,
        approver_regions: RegionGrant::Explicit(
            regions
                .iter()
                .map(|value| Region::new(value).expect("a non-blank region"))
                .collect::<BTreeSet<_>>(),
        ),
        stamp: stamp_of(approver, at(13)),
        withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
    }
}

async fn read_unit(h: &Harness, approval_id: Uuid) -> ApprovalRecord {
    let conn = h.provider.conn().expect("conn");
    approval_repo::read(&conn, &h.scope, TENANT, approval_id)
        .await
        .expect("read the unit")
        .expect("the unit is there")
}

/// Every record of one action, in `seq` order.
async fn records_of(h: &Harness, action: AuditAction) -> Vec<audit_log::Model> {
    let conn = h.provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(audit_log::Column::Action.eq(action.as_str())))
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the records")
}

/// **The expected `jsonb` half of an approval-plane record, built here.**
///
/// Spelled out as a literal rather than derived from the row, from the returned
/// `ApprovalRecord`, or from the production renderer. That is the Phase-2 G8
/// lesson stated as code: the chain re-walk rebuilt each preimage *from the
/// stored columns*, so blanking `before_state` and `after_state` left the whole
/// suite green - a test whose expectation comes out of the thing under test
/// asserts only that a value equals itself. Every key and every value below is
/// this file's own claim about what the trail must say.
fn expected_unit_state(
    state: &str,
    approver: Option<Uuid>,
    reason: Option<&str>,
    content_hash_hex: &str,
) -> serde_json::Value {
    json!({
        "approvalState": state,
        "submitterPrincipal": SUBMITTER,
        "approverPrincipal": approver,
        "reason": reason,
        "contentHash": content_hash_hex,
    })
}

/// The submitted unit's pin, rendered the way an operator reads a digest.
///
/// Read off the **record the submit returned**, which is the one thing here that
/// is not an independent expectation - and cannot be: the pin is a SHA-256 over
/// the assembled subject, so a literal would be a digest this file recomputed
/// with the code it is testing. What it does give the assertions below is a
/// value that is *wrong* if the record's `contentHash` is not the unit's own.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
        out
    })
}

async fn deny_records(h: &Harness) -> Vec<audit_log::Model> {
    let conn = h.provider.conn().expect("conn");
    audit_log::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(audit_log::Column::Action.eq(AuditAction::Deny.as_str())))
        .order_by(audit_log::Column::Seq, Order::Asc)
        .all(&conn)
        .await
        .expect("read the deny records")
}

// ---------------------------------------------------------------------------
// The world in which every refusal below means something
// ---------------------------------------------------------------------------

/// An independent reviewer, in scope, approving content nobody moved.
///
/// **Without this, every refusal below would pass against a service that
/// refuses everything.** It is also half the evidence for the pin: the digest
/// recomputed at decision time equals the one taken at submission, over a
/// subject re-derived from the database twice.
#[tokio::test]
async fn an_independent_reviewer_approves_the_content_they_were_shown() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xa1);
    let opened = submit(&h, id).await;
    assert_eq!(opened.state, ApprovalState::Submitted);
    assert_eq!(opened.content_hash.len(), 32, "a SHA-256 pin");

    let decided = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect("an independent reviewer may approve");

    assert_eq!(decided.state, ApprovalState::Approved);
    assert_eq!(decided.approver_principal, Some(APPROVER));
    assert_eq!(decided.decided_at, Some(at(13)));
    // `inst-tp-record`: the identities and the timestamps are on the record the
    // store holds, not only on the value the call returned.
    let stored = read_unit(&h, id).await;
    assert_eq!(stored, decided);
    assert_eq!(stored.submitter_principal, SUBMITTER);
    assert!(
        deny_records(&h).await.is_empty(),
        "an allowed decision is not an attempted violation"
    );
}

/// A reasoned reject lands its reason, and a withdraw names no approver.
#[tokio::test]
async fn a_reasoned_reject_and_a_withdraw_are_both_taken() {
    let h = harness().await;
    seed(&h).await;

    let rejected = Uuid::from_u128(0xa2);
    submit(&h, rejected).await;
    let record = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: rejected,
                decision: DecisionBy::Reject(APPROVER),
                reason: Some("margin below floor".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::from([
                    Region::new("eu").unwrap()
                ])),
                stamp: stamp_of(APPROVER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("a reasoned reject is taken");
    assert_eq!(record.state, ApprovalState::Rejected);
    assert_eq!(record.reason.as_deref(), Some("margin below floor"));

    let withdrawn = Uuid::from_u128(0xa3);
    submit(&h, withdrawn).await;
    let record = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: withdrawn,
                decision: DecisionBy::Void(None),
                reason: Some("withdrawn by the submitter".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::new()),
                stamp: stamp_of(SUBMITTER, at(14)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the submitter may withdraw");
    assert_eq!(record.state, ApprovalState::Voided);
    assert_eq!(record.approver_principal, None);
}

// ---------------------------------------------------------------------------
// inst-tp-distinct + inst-tp-selfaudit
// ---------------------------------------------------------------------------

/// `inst-tp-distinct` **and** `inst-tp-selfaudit`: rejected *and* recorded.
///
/// The audit assertion is the half that is easy to leave out and is the half the
/// instruction is about — a 403 with no record is a self-approval attempt that
/// leaves no trace, on the one store D-12 confines to the Auditor.
#[tokio::test]
async fn a_self_approval_is_refused_and_the_attempt_is_recorded() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xa4);
    submit(&h, id).await;

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, SUBMITTER, &["eu"]))
        .await
        .expect_err("the submitter may not approve their own unit");
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "got {err:?}"
    );

    // The record exists, and it is about this attempt.
    let records = deny_records(&h).await;
    assert_eq!(records.len(), 1, "exactly one attempted-violation record");
    let record = &records[0];
    assert_eq!(record.action, "deny");
    assert_eq!(record.actor_principal_id, SUBMITTER);
    assert_eq!(record.subject_kind, "plan_revision");
    assert_eq!(
        record.subject_ref,
        format!("{}/{}", plan_id(), seeded.revision)
    );
    assert_eq!(record.approval_ref, Some(id));
    // The chain segment is the plan's (D-135), so the attempt sits beside the
    // mutations it was about rather than on a chain of its own.
    assert_eq!(record.chain_id, plan_id().get());
    // The pair is read differently for a `deny`; see `infra::approval`'s doc.
    let after = record.after_state.as_ref().expect("the attempt");
    assert_eq!(after["refusedWith"], json!("SELF_APPROVAL_FORBIDDEN"));
    assert_eq!(after["attemptedDecision"], json!("approved"));
    assert_eq!(after["attemptedBy"], json!(SUBMITTER));
    let before = record
        .before_state
        .as_ref()
        .expect("the record as it stood");
    assert_eq!(before["approvalState"], json!("submitted"));

    // And the unit is untouched: refused means refused, not half-applied.
    let stored = read_unit(&h, id).await;
    assert_eq!(stored.state, ApprovalState::Submitted);
    assert_eq!(stored.approver_principal, None);
    assert_eq!(stored.decided_at, None);
}

/// The same principal cannot reject its own unit either — and that attempt is
/// recorded too.
#[tokio::test]
async fn a_self_rejection_is_refused_and_recorded() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xa5);
    submit(&h, id).await;

    let err = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Reject(SUBMITTER),
                reason: Some("changed my mind".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::from([
                    Region::new("eu").unwrap()
                ])),
                stamp: stamp_of(SUBMITTER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect_err("the submitter may not reject their own unit");
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "got {err:?}"
    );
    assert_eq!(deny_records(&h).await.len(), 1);
}

/// **The `deny` record carries the refused call's own correlation** (D-178).
///
/// The one record on this plane whose correlation nothing else constrains.
/// Every other record of a decision is written beside a unit whose trail an
/// equality can be taken against, and the authoring plane has
/// `rest_plans.rs::two_records_of_one_patch_carry_one_correlation_id` for the
/// same job. The `deny` is written alone, on the one path a later group would
/// most plausibly reach for a fresh `Uuid::now_v7()` on — and until this test,
/// replacing `request.stamp.correlation_id` with exactly that left the whole
/// suite green.
///
/// The stamp carries a value **this test alone** uses, rather than the binary's
/// `TEST_CORRELATION`. A fresh mint fails either way; a record wired to some
/// other call's correlation fails only this way.
#[tokio::test]
async fn the_deny_record_carries_the_refused_calls_own_correlation() {
    const REFUSED_CALL: Uuid = Uuid::from_u128(0x_de_11_c0_44);

    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xa4_c0);
    submit(&h, id).await;

    let mut request = approve_as(id, SUBMITTER, &["eu"]);
    request.stamp.correlation_id = REFUSED_CALL;
    let err = h
        .approvals
        .decide(&h.scope, TENANT, request)
        .await
        .expect_err("the submitter may not approve their own unit");
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "got {err:?}"
    );

    let records = deny_records(&h).await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].correlation_id,
        Some(REFUSED_CALL),
        "the attempted-violation record must name the call that made the attempt"
    );
    assert_ne!(
        records[0].correlation_id,
        Some(TEST_CORRELATION),
        "and not the value every other record in this binary happens to carry"
    );
}

// ---------------------------------------------------------------------------
// inst-as-reject's mandatory reason — REASON_REQUIRED
// ---------------------------------------------------------------------------

/// `REASON_REQUIRED`, at the surface rather than as a storage fault.
///
/// **This test is the inversion of `sqlite_approval_repo.rs`'s
/// `a_reject_without_a_reason_is_still_refused_by_the_check_beneath_the_code`**,
/// which records that the store answers and the code is owed. It still does;
/// what changed is that nothing reaches it, because the surface answers first
/// with the code §5 declares.
#[tokio::test]
async fn a_reject_with_no_reason_is_refused_with_the_code_and_not_as_a_storage_fault() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xa6);
    submit(&h, id).await;

    let err = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Reject(APPROVER),
                reason: None,
                approver_regions: RegionGrant::Explicit(BTreeSet::from([
                    Region::new("eu").unwrap()
                ])),
                stamp: stamp_of(APPROVER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect_err("a reject needs a reason");
    assert!(matches!(err, DomainError::ReasonRequired(_)), "got {err:?}");
    // Not a storage fault, which is what the inverted test recorded: the
    // constraint name must not appear at all.
    assert!(
        !err.to_string().contains("chk_pricing_approval_reason"),
        "the CHECK must no longer be what answers: {err}"
    );

    // A malformed request is **not** an attempted violation. Recording it would
    // bury the two that are under an impatient client's retries.
    assert!(deny_records(&h).await.is_empty());
}

// ---------------------------------------------------------------------------
// inst-ap-scope — REGION_SCOPE_DENIED
// ---------------------------------------------------------------------------

/// `inst-ap-scope`: the grant must cover every pricing region of the pinned
/// change set, and the attempt is audited.
///
/// # This case and its sibling drive `RegionGrant::Explicit`, which no HTTP
/// request produces
///
/// The gear's only production construction site of [`DecideRequest`]
/// (`api::rest::approvals::decide_record`) fills `approver_regions` from
/// `region_grant_of_this_surface()`, which returns
/// [`RegionGrant::Untransported`] because no document in the set says how a
/// pricing-region grant is transported. Under that variant `judge` derives the
/// grant *from the change set itself*, so the subset test can never fail and
/// `REGION_SCOPE_DENIED` is unreachable over the wire — measured here by
/// [`the_reachable_grant_does_not_enforce_the_region_rule`] below, which drives
/// this same world through the variant the surface actually sends and gets an
/// approve.
///
/// So the two `Explicit` cases certify the *rule*, not a reachable refusal. The
/// day the transport is declared, `region_grant_of_this_surface` returns
/// `Explicit` and these become reachability tests too.
#[tokio::test]
async fn an_out_of_scope_approver_is_refused_and_the_attempt_is_recorded() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xa7);
    submit(&h, id).await;

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["us"]))
        .await
        .expect_err("a us-scoped reviewer cannot approve an eu row");
    assert!(
        matches!(err, DomainError::RegionScopeDenied(_)),
        "got {err:?}"
    );

    let records = deny_records(&h).await;
    assert_eq!(records.len(), 1);
    let after = records[0].after_state.as_ref().expect("the attempt");
    assert_eq!(after["refusedWith"], json!("REGION_SCOPE_DENIED"));
    assert_eq!(records[0].actor_principal_id, APPROVER);

    assert_eq!(read_unit(&h, id).await.state, ApprovalState::Submitted);
}

/// The region compared is the **price row's own axis**, not anything about the
/// caller's tenancy.
///
/// A second row in a second market is added, and the grant that covered the
/// first no longer covers the change set. Nothing about the principal changed —
/// which is the executable form of "pricing `region` is a commercial axis, never
/// conflated with the authz-region claim" (`inst-rb-region`, S4 C5): the only
/// thing that moved is a column on a price row.
#[tokio::test]
async fn the_regions_compared_are_the_price_rows_own() {
    let h = harness().await;
    seed(&h).await;

    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: Uuid::from_u128(0xb_0002),
                scope_key: scope_key("us"),
                content: flat_row(),
                created_by: SUBMITTER,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author a second market");

    let id = Uuid::from_u128(0xa8);
    submit(&h, id).await;

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect_err("the change set now touches us as well");
    assert!(
        matches!(err, DomainError::RegionScopeDenied(_)),
        "got {err:?}"
    );

    // And the same principal with the wider grant is allowed, so the refusal is
    // about coverage rather than about the principal.
    //
    // The refused unit is **withdrawn** first, and that is not tidying: a
    // refused decision leaves the record `submitted`, and a `submitted` unit
    // holds the plan's subject (`inst-co-single-pending`) — so the second submit
    // below is refused `PENDING_CHANGE_UNIT_EXISTS` until the first is closed.
    // `inst-as-void`'s withdraw is the escape, and this is it in use.
    h.approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Void(Some(SUBMITTER)),
                reason: None,
                approver_regions: RegionGrant::Explicit(BTreeSet::new()),
                stamp: stamp_of(SUBMITTER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("the submitter withdraws the unit they opened");

    let wider = Uuid::from_u128(0xa9);
    submit(&h, wider).await;
    h.approvals
        .decide(&h.scope, TENANT, approve_as(wider, APPROVER, &["eu", "us"]))
        .await
        .expect("a grant covering both markets approves");
}

/// The variant the HTTP surface actually sends **cannot** produce the refusal
/// the two cases above assert.
///
/// Same world as [`an_out_of_scope_approver_is_refused_and_the_attempt_is_recorded`]
/// — an `eu` change set, a reviewer whose grant covers nothing — with the one
/// difference that `approver_regions` is [`RegionGrant::Untransported`], which
/// `api::rest::approvals::region_grant_of_this_surface` returns unconditionally
/// and is therefore the only variant any HTTP request can carry. `judge`
/// resolves it to the change set's own reach, the subset test holds by
/// construction, and the approve lands.
///
/// This is the honest half of `inst-ap-scope`'s coverage: the rule is built and
/// judged, and **not enforced on the reachable path**. Replacing
/// `RegionGrant::Explicit` with this variant in either case above turns its
/// `expect_err` into a panic, which is what makes this assertion load-bearing
/// rather than decorative.
#[tokio::test]
async fn the_reachable_grant_does_not_enforce_the_region_rule() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xaa);
    submit(&h, id).await;

    h.approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Approve(APPROVER),
                reason: None,
                // The grant a request off the wire carries, verbatim.
                approver_regions: RegionGrant::Untransported,
                stamp: stamp_of(APPROVER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("an untransported grant covers whatever the change set touches");

    assert_eq!(read_unit(&h, id).await.state, ApprovalState::Approved);
    assert!(
        deny_records(&h).await.is_empty(),
        "no scope violation can be recorded on a path that cannot refuse one"
    );
}

// ---------------------------------------------------------------------------
// inst-as-immutable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_decision_on_a_decided_unit_is_refused_as_not_pending() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xaa);
    submit(&h, id).await;
    h.approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect("the first decision lands");

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect_err("a decided unit takes no second decision");
    assert!(
        matches!(err, DomainError::ApprovalNotPending(_)),
        "got {err:?}"
    );
    // A race an entitled reviewer lost is not an attempted violation.
    assert!(deny_records(&h).await.is_empty());
}

// ---------------------------------------------------------------------------
// inst-ap-pin — the re-verification arm
// ---------------------------------------------------------------------------

/// `APPROVAL_CONTENT_MISMATCH`, staged with **every row version coinciding**.
///
/// The record is opened directly on the store with a pin that does not match the
/// subject, and then nothing at all is touched: the plan revision and the price
/// row stand at exactly the versions they were seeded at, asserted below before
/// the approve is attempted. So no compare-and-swap, no `If-Match` and no
/// version comparison anywhere in the stack can be what answers — the only
/// difference between this world and the approving one at the top of the file is
/// the 32 bytes in `content_hash`.
///
/// See the module doc for why the mismatch cannot be staged by mutating the
/// subject: the TOCTOU void closes the record first, which is the guard in front
/// of this one working.
#[tokio::test]
async fn an_approve_whose_pin_does_not_match_the_subject_is_refused() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xab);

    let conn = h.provider.conn().expect("conn");
    approval_repo::open(
        &conn,
        &h.scope,
        NewApproval {
            approval_id: id,
            tenant_id: TENANT,
            subject_ref: format!("{}/{}", plan_id(), seeded.revision),
            subject_kind: bss_pricing::domain::audit::AuditSubjectKind::PlanRevision,
            // A pin over content that is not there. Thirty-two bytes, so the
            // shape is right and only the value is wrong.
            content_hash: vec![0_u8; 32],
            materiality: json!({}),
            // The key the plan's one row sits on, exactly as a real submit would
            // hold it (`inst-co-single-pending`). Empty would be simpler and would
            // stage a unit no surface can produce — and the register is what a
            // *second* submit in this file then contends with.
            held_keys: BTreeSet::from([scope_key("eu").to_string()]),
        },
        stamp_of(SUBMITTER, at(12)),
    )
    .await
    .expect("open a unit whose pin does not match");

    // The versions coincide — nothing has moved since the seed.
    let revision = h
        .plans
        .find_revision(&h.scope, TENANT, plan_id(), seeded.revision)
        .await
        .expect("read the revision")
        .expect("the revision is there");
    assert_eq!(revision.row_version, seeded.revision_version);
    let row = h
        .prices
        .find(&h.scope, TENANT, seeded.price_id)
        .await
        .expect("read the row")
        .expect("the row is there");
    assert_eq!(row.row_version, seeded.price_version);

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect_err("the pin does not match");
    assert!(
        matches!(err, DomainError::ApprovalContentMismatch(_)),
        "got {err:?}"
    );
    assert_eq!(read_unit(&h, id).await.state, ApprovalState::Submitted);
    // A lost race, not an attempted violation.
    assert!(deny_records(&h).await.is_empty());
}

/// A **reject** is not pin-checked (§2 step 2a puts the re-verification on
/// approve), so the same unit can still be rejected.
///
/// It is here rather than only in the domain tests because it is the arm a
/// too-eager implementation breaks: a reviewer who can neither approve nor
/// reject a unit whose subject moved has no way to close it at all.
#[tokio::test]
async fn a_reject_is_not_refused_by_a_stale_pin() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xac);
    let conn = h.provider.conn().expect("conn");
    approval_repo::open(
        &conn,
        &h.scope,
        NewApproval {
            approval_id: id,
            tenant_id: TENANT,
            subject_ref: format!("{}/{}", plan_id(), seeded.revision),
            subject_kind: bss_pricing::domain::audit::AuditSubjectKind::PlanRevision,
            content_hash: vec![0_u8; 32],
            materiality: json!({}),
            held_keys: BTreeSet::from([scope_key("eu").to_string()]),
        },
        stamp_of(SUBMITTER, at(12)),
    )
    .await
    .expect("open a unit whose pin does not match");

    let record = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Reject(APPROVER),
                reason: Some("margin below floor".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::from([
                    Region::new("eu").unwrap()
                ])),
                stamp: stamp_of(APPROVER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("a reject is not pin-checked");
    assert_eq!(record.state, ApprovalState::Rejected);
}

// ---------------------------------------------------------------------------
// inst-ap-pin / inst-as-void — the TOCTOU guard, one test per surface
// ---------------------------------------------------------------------------

/// Every mutating surface's void, asserted the same way.
async fn assert_voided(h: &Harness, approval_id: Uuid) {
    let stored = read_unit(h, approval_id).await;
    assert_eq!(
        stored.state,
        ApprovalState::Voided,
        "the pinned subject moved and the unit is still open"
    );
    assert_eq!(stored.approver_principal, None, "a void has no decider");
    assert_eq!(stored.reason.as_deref(), Some(TOCTOU_VOID_REASON));
    assert!(stored.decided_at.is_some());
}

#[tokio::test]
async fn patching_the_plan_facet_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb1);
    submit(&h, id).await;

    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("patch the plan facet");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn replacing_the_phase_graph_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb2);
    submit(&h, id).await;

    h.shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            vec![PlanPhase {
                phase_id: terminal_phase(),
                kind: PhaseKind::Evergreen,
                display_name: None,
                ordinal: 0,
                converts_to_phase_id: None,
                phase_duration_days: None,
                display_trial_days: None,
            }],
            stamp(),
        )
        .await
        .expect("replace the phase graph");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn replacing_the_addon_rules_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb3);
    submit(&h, id).await;

    h.shapes
        .replace_addon_rules(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            vec![AddonRule {
                addon_sku_id: Uuid::from_u128(0x21),
                required: false,
                min_qty: None,
                max_qty: Some(1),
                step_qty: None,
                price_override_ref: None,
                depends_on: Vec::new(),
                conflicts_with: Vec::new(),
            }],
            stamp(),
        )
        .await
        .expect("replace the add-on rules");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn setting_the_descriptor_set_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb4);
    submit(&h, id).await;

    h.shapes
        .set_descriptor_set(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            DescriptorSet {
                invoice_line_template: Some("{plan}".to_owned()),
                gl_code: Some("4001".to_owned()),
                itemization_rule: Some("per_charge".to_owned()),
                additional: BTreeMap::new(),
            },
            stamp(),
        )
        .await
        .expect("replace the descriptor set");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn abandoning_the_draft_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb5);
    submit(&h, id).await;

    h.plans
        .abandon_draft(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            stamp(),
        )
        .await
        .expect("abandon the draft");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn creating_a_price_row_voids_the_pending_unit() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xb6);
    submit(&h, id).await;

    h.prices
        .create_draft(
            &h.scope,
            TENANT,
            NewPriceDraft {
                price_id: Uuid::from_u128(0xb_0003),
                scope_key: scope_key("us"),
                content: flat_row(),
                created_by: SUBMITTER,
                created_at_utc: at(10),
                correlation_id: TEST_CORRELATION,
            },
        )
        .await
        .expect("author another row");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn patching_a_price_row_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb7);
    submit(&h, id).await;

    let mut content = flat_row();
    content.row.amount_minor = Some(MinorAmount::new(8_800).expect("a non-negative amount"));
    h.prices
        .update_draft(
            &h.scope,
            TENANT,
            seeded.price_id,
            seeded.price_version,
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("patch the row");

    assert_voided(&h, id).await;
}

#[tokio::test]
async fn deleting_a_price_row_voids_the_pending_unit() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xb8);
    submit(&h, id).await;

    h.prices
        .delete_draft(
            &h.scope,
            TENANT,
            seeded.price_id,
            seeded.price_version,
            stamp(),
            None,
        )
        .await
        .expect("delete the draft row");

    assert_voided(&h, id).await;
}

/// **The surface that must not void**, and the one claim in this file that a
/// removal cannot prove.
///
/// `create_plan` mints a plan and its revision `0`. No approval can be pinned to
/// a subject that did not exist when it was opened, so the "no" row of the
/// enumeration holds structurally.
///
/// **Stated because it was measured and is not what it looks like:** wiring the
/// void into `plan_repo::create_draft_on` — the exact defect this test appears
/// to guard against — reddens **nothing**, this test included. It cannot: the
/// void is keyed per plan, and a plan being created has no pending unit of its
/// own by construction, while a unit of *another* plan is out of the prefix's
/// reach. There is no reachable world in which a plan create could void
/// anything, so there is nothing to execute.
///
/// What *is* provable by removal is the per-plan keying itself, and that is the
/// next test: dropping the `subject_ref` prefix predicate reddens
/// `a_mutation_of_another_plan_does_not_void_this_plans_unit` and nothing else.
/// This test stays as the executable statement of the enumeration's "no" row —
/// it would catch a void that closed units tenant-wide *and* ran on creation —
/// and its weakness is written down rather than left for a reader to assume it
/// is stronger than it is.
#[tokio::test]
async fn creating_another_plan_does_not_void_a_pending_unit() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xb9);
    submit(&h, id).await;

    h.plans
        .create_draft(&h.scope, new_plan_draft(other_plan_id()))
        .await
        .expect("create a second plan");

    let stored = read_unit(&h, id).await;
    assert_eq!(
        stored.state,
        ApprovalState::Submitted,
        "creating a plan closed a unit pinned to a different one"
    );
}

/// A mutation of **another plan** leaves this plan's unit alone.
///
/// The prefix match is what makes the void a per-plan act, and this is the test
/// that fails if it is ever widened to the tenant.
#[tokio::test]
async fn a_mutation_of_another_plan_does_not_void_this_plans_unit() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xba);
    submit(&h, id).await;

    let other = h
        .plans
        .create_draft(&h.scope, new_plan_draft(other_plan_id()))
        .await
        .expect("create a second plan");
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            other_plan_id(),
            other.revision,
            other.row_version,
            PlanShapePatch {
                plan_tier: Some("bronze".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("patch the other plan");

    assert_eq!(read_unit(&h, id).await.state, ApprovalState::Submitted);
}

/// Flip a revision to `published` **straight at the table**.
///
/// The one thing in this suite that reaches a plan without passing the void
/// rail, which is the whole point: it stands in for the publish commit, which
/// deliberately does not void (`void_pending_units_of`'s enumeration) and whose
/// real version lives in `tests/sqlite_publish_commit.rs` behind a registry
/// double this suite has no other use for.
async fn publish_revision_at_the_table(h: &Harness, revision: u64) {
    let conn = h.provider.conn().expect("conn");
    let result = plan::Entity::update_many()
        .secure()
        .scope_with(&h.scope)
        .col_expr(
            plan::Column::LifecycleState,
            Expr::value(LifecycleState::Published.as_str()),
        )
        .filter(
            Condition::all()
                .add(plan::Column::PlanId.eq(plan_id().get()))
                .add(plan::Column::Revision.eq(i64::try_from(revision).expect("a small revision"))),
        )
        .exec(&conn)
        .await
        .expect("flip the lifecycle state");
    assert_eq!(result.rows_affected, 1, "the flip must have moved one row");
}

/// **Opening the successor revision voids the unit the publish orphaned.**
///
/// `open_revision` appends its own audit record instead of going through
/// `record_revision_mutation`, so it inherits nothing and had to be wired
/// explicitly — the surface the void's enumeration did not list until
/// 2026-08-04.
///
/// What it closes is only the orphan, and the staging says so: a unit can be
/// pinned to nothing but a plan's open draft, and `open_revision` refuses while
/// one exists, so the only unit it can ever meet is one whose pinned draft has
/// since published. The unit is asserted still `submitted` after the flip, so
/// the void below is this call's and not the flip's.
///
/// **The publish is staged as a bare lifecycle flip on the table, and since
/// 2026-08-04 that is what keeps this test about `open_revision`.**
/// `PublishService::commit` now voids the units it did not consume, so a staging
/// that went through the commit would leave nothing here to find and this test
/// would pass against an `open_revision` that voids nothing at all. The flip
/// touches the row and no approval record, which is the world in which this
/// call is the only thing that can answer.
#[tokio::test]
async fn opening_the_successor_revision_voids_the_unit_the_publish_orphaned() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xbf);
    submit(&h, id).await;

    publish_revision_at_the_table(&h, seeded.revision).await;
    assert_eq!(
        read_unit(&h, id).await.state,
        ApprovalState::Submitted,
        "a bare lifecycle flip touches no approval record, so whatever voids \
         below is the call under test"
    );

    h.plans
        .open_revision(&h.scope, TENANT, plan_id(), stamp_of(SUBMITTER, at(15)))
        .await
        .expect("open the successor revision");

    assert_voided(&h, id).await;
}

/// A **decided** unit is not re-voided, and this is not cosmetic.
///
/// `inst-as-immutable` is enforced by `trg_pricing_approval_append_only`, so a
/// void that reached a decided record would be refused by the trigger — and
/// because the void runs inside the mutation's transaction, that refusal would
/// roll the *mutation* back. Every authoring edit to a plan that had ever been
/// approved would fail. The `state = 'submitted'` predicate is what stops it,
/// and this is the test that would catch its removal.
#[tokio::test]
async fn a_decided_unit_is_not_touched_by_a_later_mutation() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xbb);
    submit(&h, id).await;
    h.approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect("approve it");

    let before = read_unit(&h, id).await;
    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("a later mutation must still be possible");

    assert_eq!(read_unit(&h, id).await, before);
}

/// After a void, a fresh submit opens a **new** record (`inst-as-void`,
/// `inst-as-immutable`).
#[tokio::test]
async fn a_fresh_submit_after_a_void_opens_a_new_record() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let first = Uuid::from_u128(0xbc);
    submit(&h, first).await;

    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            PlanShapePatch {
                plan_tier: Some("silver".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp(),
        )
        .await
        .expect("patch the plan");
    assert_voided(&h, first).await;

    let second = Uuid::from_u128(0xbd);
    let reopened = submit(&h, second).await;
    assert_eq!(reopened.state, ApprovalState::Submitted);
    assert_ne!(
        reopened.content_hash,
        read_unit(&h, first).await.content_hash,
        "the new unit pins the content as it now stands"
    );

    h.approvals
        .decide(&h.scope, TENANT, approve_as(second, APPROVER, &["eu"]))
        .await
        .expect("the new unit approves against the new content");
}

/// A decision on a unit the guard has already voided is `APPROVAL_NOT_PENDING`.
///
/// This is the path a real TOCTOU takes end to end: submit, somebody edits, the
/// reviewer's approve arrives — and the answer is the 409 §5 names rather than a
/// decision over content that moved.
#[tokio::test]
async fn approving_a_unit_the_guard_voided_is_refused_as_not_pending() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xbe);
    submit(&h, id).await;

    let mut content = flat_row();
    content.row.amount_minor = Some(MinorAmount::new(1).expect("a non-negative amount"));
    h.prices
        .update_draft(
            &h.scope,
            TENANT,
            seeded.price_id,
            seeded.price_version,
            content,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("somebody edits the row");

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect_err("the unit was voided by the edit");
    assert!(
        matches!(err, DomainError::ApprovalNotPending(_)),
        "got {err:?}"
    );
}

/// The premise the price-row void cases above rest on: `seed` leaves the row
/// **draft**.
///
/// Not a test of the void rule, and the name says which it is. `update_draft`
/// and `delete_draft` refuse a non-draft row with `NotDraft`, so a seed that
/// left it published would not make those cases green over a condition the rule
/// never meets — it would panic them on their own `.expect`. What this pins is
/// the cheaper thing: which fixture they are standing on, read once, where the
/// alternative is eleven cases each asserting it again.
///
/// **The publish path is a void site**, and nothing here says otherwise:
/// `PublishService::commit` voids what it did not consume, under
/// `ORPHANED_BY_PUBLISH_REASON`, and `tests/rest_publish.rs` asserts it.
#[tokio::test]
async fn the_seed_leaves_the_row_draft_which_is_what_the_void_cases_above_assume() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let row = h
        .prices
        .find(&h.scope, TENANT, seeded.price_id)
        .await
        .expect("read the row")
        .expect("the row is there");
    assert_eq!(row.lifecycle_state, LifecycleState::Draft);
}

// ---------------------------------------------------------------------------
// The approval trail — `inst-tp-record`, `inst-as-void`, and the stamp the
// Global Constraints require of every mutating repository call.
// ---------------------------------------------------------------------------

/// Opening a unit is a mutation of `pricing_approval`, so it records.
///
/// **The token is minted here and owed to S5 §6** — the roster has no verb for
/// the submit, which `domain::audit`'s module doc reports. What the test pins is
/// the record, not the spelling's provenance.
#[tokio::test]
async fn a_submit_lands_its_own_record() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xd1);
    let opened = submit(&h, id).await;

    let records = records_of(&h, AuditAction::Submit).await;
    assert_eq!(records.len(), 1, "one submit, one record");
    let record = &records[0];
    assert_eq!(record.actor_principal_id, SUBMITTER);
    assert_eq!(record.recorded_at, at(12));
    assert_eq!(record.approval_ref, Some(id));
    assert_eq!(
        record.subject_ref,
        format!("{}/{}", plan_id(), seeded.revision)
    );
    assert_eq!(record.subject_kind, "plan_revision");
    // D-135: the approval's aggregate is the plan, so the unit's life extends the
    // same segment as the mutations it is about.
    assert_eq!(record.chain_id, plan_id().get());
    assert_eq!(record.correlation_id, Some(TEST_CORRELATION));
    // Nothing held this subject before the insert, and an absence is the only
    // honest rendering of that.
    assert_eq!(record.before_state, None);
    assert_eq!(
        record.after_state.as_ref().expect("the opened unit"),
        &expected_unit_state("submitted", None, None, &hex(&opened.content_hash))
    );
}

/// An approve records who agreed and what the unit moved to (`inst-tp-record`).
#[tokio::test]
async fn an_approve_lands_a_record_naming_the_reviewer_and_the_unit_it_moved() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xd2);
    let opened = submit(&h, id).await;
    h.approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect("an independent reviewer may approve");

    let records = records_of(&h, AuditAction::Approve).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.actor_principal_id, APPROVER);
    assert_eq!(record.recorded_at, at(13));
    assert_eq!(record.approval_ref, Some(id));
    assert_eq!(record.chain_id, plan_id().get());
    assert_eq!(record.correlation_id, Some(TEST_CORRELATION));
    let pin = hex(&opened.content_hash);
    // The pair is the **unit's**, before and after the flip — see
    // `infra::approval`'s Ruling 2. The subject did not move; the judgement did.
    assert_eq!(
        record.before_state.as_ref().expect("the unit as it stood"),
        &expected_unit_state("submitted", None, None, &pin)
    );
    assert_eq!(
        record.after_state.as_ref().expect("the decided unit"),
        &expected_unit_state("approved", Some(APPROVER), None, &pin)
    );
}

/// A reject records its mandatory reason (`inst-as-reject`).
#[tokio::test]
async fn a_reject_records_the_reason_the_rule_makes_mandatory() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xd3);
    let opened = submit(&h, id).await;
    h.approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Reject(APPROVER),
                reason: Some("margin below floor".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::from([
                    Region::new("eu").unwrap()
                ])),
                stamp: stamp_of(APPROVER, at(13)),
                withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
            },
        )
        .await
        .expect("a reasoned reject is taken");

    let records = records_of(&h, AuditAction::Reject).await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.actor_principal_id, APPROVER);
    let pin = hex(&opened.content_hash);
    assert_eq!(
        record.before_state.as_ref().expect("the unit as it stood"),
        &expected_unit_state("submitted", None, None, &pin)
    );
    // The reason is on the trail as well as on the row: `inst-tp-record` puts the
    // identities, the timestamps and the grounds in **both** places, and an
    // auditor reading the trail alone must not have to join to the store to learn
    // why a change set was refused.
    assert_eq!(
        record.after_state.as_ref().expect("the decided unit"),
        &expected_unit_state("rejected", Some(APPROVER), Some("margin below floor"), &pin)
    );
}

/// **The withdrawer's identity, which the approval row cannot hold.**
///
/// `chk_pricing_approval_distinct_principals` refuses the submitter in
/// `approver_principal` — so `judge` writes `approver()`, which is `None` on
/// every void, and until the trail took the stamp the actor was simply lost. A
/// `voided` unit then answered "who withdrew this" with nothing,
/// indistinguishable from the TOCTOU guard closing it, which is the *other* way
/// a unit reaches `voided` and the one no principal asked for.
///
/// **The withdrawer here is the `CatalogAdmin`, not the submitter, and the
/// difference is the whole test.** `inst-as-void` names *"the submitter (or a
/// `CatalogAdmin`)"*, and only the second of those is an identity nothing else
/// on the record recovers: when the submitter withdraws their own unit, the
/// asserted actor is also `record.submitter_principal` and is already rendered
/// in `after_state.submitterPrincipal`, so this test passed against an
/// `append_trail` that wrote `record.submitter_principal` instead of the stamp's
/// actor — which is to say it did not test what it is named for. Every other
/// withdraw in the crate is the submitter's (`rest_approvals.rs`,
/// `rest_publish.rs`), so this is the one place the third-party case is driven.
#[tokio::test]
async fn a_withdraw_records_the_principal_the_approval_row_cannot_name() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xd4);
    let opened = submit(&h, id).await;
    let record = h
        .approvals
        .decide(
            &h.scope,
            TENANT,
            DecideRequest {
                approval_id: id,
                decision: DecisionBy::Void(Some(CATALOG_ADMIN)),
                reason: Some("superseded by a later change set".to_owned()),
                approver_regions: RegionGrant::Explicit(BTreeSet::new()),
                stamp: stamp_of(CATALOG_ADMIN, at(14)),
                withdraw_authority: WithdrawAuthority::CatalogAuthority,
            },
        )
        .await
        .expect("a CatalogAdmin may withdraw a unit they did not submit");

    // The half the store cannot say.
    assert_eq!(record.state, ApprovalState::Voided);
    assert_eq!(
        record.approver_principal, None,
        "the column is named for a different role and the CHECK refuses the submitter in it"
    );
    assert_eq!(
        record.submitter_principal, SUBMITTER,
        "and the submitter is somebody else, or the assertion below is a tautology"
    );

    // The half the trail says.
    let records = records_of(&h, AuditAction::Withdraw).await;
    assert_eq!(records.len(), 1);
    let trail = &records[0];
    assert_eq!(
        trail.actor_principal_id, CATALOG_ADMIN,
        "the withdrawer is on the trail even though no column can hold them, and no other \
         field of the record can be mistaken for them"
    );
    assert_eq!(trail.recorded_at, at(14));
    assert_eq!(trail.approval_ref, Some(id));
    let pin = hex(&opened.content_hash);
    assert_eq!(
        trail.after_state.as_ref().expect("the voided unit"),
        &expected_unit_state(
            "voided",
            None,
            Some("superseded by a later change set"),
            &pin
        )
    );
}

/// The guard's own void closes units and writes **nothing** — the boundary
/// `domain::audit`'s module doc draws.
///
/// A record would need an actor `inst-au-pii` makes a fact rather than a nicety,
/// and no principal asked for this void. What accounts for it is the record of
/// the mutation that triggered it, on the same segment at the same instant — so
/// this test asserts both halves: no `withdraw` record, and the triggering
/// mutation's own record present.
#[tokio::test]
async fn the_toctou_void_writes_no_withdraw_record_and_the_mutation_that_caused_it_does() {
    let h = harness().await;
    let seeded = seed(&h).await;
    let id = Uuid::from_u128(0xd5);
    submit(&h, id).await;

    h.plans
        .update_draft(
            &h.scope,
            TENANT,
            plan_id(),
            seeded.revision,
            seeded.revision_version,
            PlanShapePatch {
                plan_tier: Some("platinum".to_owned()),
                ..PlanShapePatch::default()
            },
            stamp_of(SUBMITTER, at(13)),
        )
        .await
        .expect("the facet lands");

    assert_voided(&h, id).await;
    assert_eq!(
        read_unit(&h, id).await.reason.as_deref(),
        Some(TOCTOU_VOID_REASON),
        "the guard closed it, not a person"
    );
    assert!(
        records_of(&h, AuditAction::Withdraw).await.is_empty(),
        "a machine-driven void attributes nothing to anybody"
    );
    let updates = records_of(&h, AuditAction::Update).await;
    assert!(
        updates.iter().any(|row| row.recorded_at == at(13)),
        "the mutation that voided the unit accounts for the instant on the same segment"
    );
}

/// The decision and its record are **one transaction**.
///
/// Staged by corrupting the segment head so the append cannot compute a link:
/// `plan_repo`'s `opening_a_successor_whose_record_cannot_be_written_mints_no_revision_number`
/// stages the same failure on the authoring plane, and this is its approval-plane
/// twin. What must hold is that the unit is still `submitted` afterwards — a
/// decision that committed while its trail rolled back would leave
/// `pricing_audit_log` answering "who approved this" with nothing, which is the
/// sentence `approval_repo`'s module doc opens with.
#[tokio::test]
async fn a_decision_whose_record_cannot_be_written_does_not_decide_the_unit() {
    let h = harness().await;
    seed(&h).await;
    let id = Uuid::from_u128(0xd6);
    submit(&h, id).await;

    let conn = h.provider.conn().expect("conn");
    let head = audit_log::Entity::find()
        .secure()
        .scope_with(&h.scope)
        .filter(Condition::all().add(audit_log::Column::TenantId.eq(TENANT)))
        .order_by(audit_log::Column::Seq, Order::Desc)
        .one(&conn)
        .await
        .expect("read the head")
        .expect("the segment is not empty");
    let corrupt = audit_log::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(TENANT),
        chain_id: sea_orm::ActiveValue::Set(plan_id().get()),
        seq: sea_orm::ActiveValue::Set(head.seq + 1),
        entry_kind: sea_orm::ActiveValue::Set("mutation".to_owned()),
        recorded_at: sea_orm::ActiveValue::Set(at(12)),
        actor_principal_id: sea_orm::ActiveValue::Set(SUBMITTER),
        action: sea_orm::ActiveValue::Set("submit".to_owned()),
        subject_kind: sea_orm::ActiveValue::Set("plan_revision".to_owned()),
        subject_ref: sea_orm::ActiveValue::Set(format!("{}/0", plan_id())),
        before_state: sea_orm::ActiveValue::Set(None),
        after_state: sea_orm::ActiveValue::Set(None),
        approval_ref: sea_orm::ActiveValue::Set(None),
        correlation_id: sea_orm::ActiveValue::Set(None),
        segment_heads: sea_orm::ActiveValue::Set(None),
        prev_hash: sea_orm::ActiveValue::Set(None),
        // Not 32 bytes: the next append cannot link to it.
        row_hash: sea_orm::ActiveValue::Set(vec![0_u8]),
    };
    audit_log::Entity::insert(corrupt.clone())
        .secure()
        .scope_with_model(&h.scope, &corrupt)
        .expect("scope the corrupt head")
        .exec(&conn)
        .await
        .expect("plant the corrupt head");

    let err = h
        .approvals
        .decide(&h.scope, TENANT, approve_as(id, APPROVER, &["eu"]))
        .await
        .expect_err("the decision cannot be recorded, so it does not happen");
    assert!(matches!(err, DomainError::Internal(_)), "got {err:?}");

    let stored = read_unit(&h, id).await;
    assert_eq!(
        stored.state,
        ApprovalState::Submitted,
        "the swap rolled back with the record it could not write"
    );
    assert_eq!(stored.approver_principal, None);
    assert_eq!(stored.decided_at, None);
    assert!(records_of(&h, AuditAction::Approve).await.is_empty());
}

// ---------------------------------------------------------------------------
// The plan-revision half of `inst-co-single-pending`, which the key register
// cannot answer
// ---------------------------------------------------------------------------

/// **A revision with no price row holds no key, and a second unit over it is still
/// refused** — the statement only the kept `<plan_id>/` prefix match can refuse.
///
/// The widening to a key register replaced a plan-revision lock, and
/// `find_pending_for_plan` was kept rather than deleted. Its own doc says why: *"a plan
/// revision with no price row on it holds no key at all"*, so its register contribution
/// is empty, `refuse_held_key` answers `None` **without a query**, and two reviewers
/// would otherwise decide two records over one revision — with the order of arrival
/// deciding what publishes. Nothing tested that, so the sentence was an argument rather
/// than a property.
///
/// **No neighbour can refuse this input.** The register is silent by construction (an
/// empty key set), the per-subject check is silent (it is scoped to non-plan-revision
/// subjects — a plan unit's `subject_ref` is `<plan_id>/<revision>`, which is exactly
/// what the prefix match reads), and the store's own constraints admit two rows freely:
/// the primary key is the caller-minted `approval_id`. Delete the prefix match and this
/// is the only assertion that changes.
///
/// The plan here is therefore seeded **without** `seed`'s price row: a phase chain and a
/// descriptor set, which is all `publish::assemble` needs to answer.
#[tokio::test]
async fn a_second_unit_over_a_rowless_revision_is_refused_by_the_plan_prefix() {
    let h = harness().await;
    let created = h
        .plans
        .create_draft(&h.scope, new_plan_draft(plan_id()))
        .await
        .expect("create the draft");
    h.shapes
        .replace_phases(
            &h.scope,
            TENANT,
            plan_id(),
            created.revision,
            created.row_version,
            vec![PlanPhase {
                phase_id: terminal_phase(),
                kind: PhaseKind::Evergreen,
                display_name: None,
                ordinal: 0,
                converts_to_phase_id: None,
                phase_duration_days: None,
                display_trial_days: None,
            }],
            stamp(),
        )
        .await
        .expect("attach the phase chain");

    let first = Uuid::from_u128(0x_a1_01);
    submit(&h, first).await;

    // The premise: the unit really holds nothing, so nothing about the register can be
    // what refuses below. Asserted rather than assumed — a register that had held a
    // key would make this test pass for the widening's reason instead.
    let conn = h.provider.conn().expect("conn");
    assert!(
        approval_repo::held_keys_of(&conn, &h.scope, TENANT, first)
            .await
            .expect("read the register")
            .is_empty(),
        "a revision with no price row touches no canonical scope key"
    );

    let refused = h
        .approvals
        .submit(
            &h.scope,
            TENANT,
            plan_id(),
            Uuid::from_u128(0x_a1_02),
            json!({ "reason": "noConfiguredThreshold" }),
            stamp_of(SUBMITTER, at(12)),
        )
        .await
        .expect_err("one revision, one pending unit");

    match refused {
        DomainError::PendingChangeUnitExists(detail) => {
            assert!(
                detail.contains(&plan_id().to_string()),
                "the refusal names the plan whose revision is under review: {detail}"
            );
            assert!(
                detail.contains(&first.to_string()),
                "and the unit holding it, so the operator knows what to decide: {detail}"
            );
        }
        other => panic!("expected PENDING_CHANGE_UNIT_EXISTS, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The supersession unit (D-88's `inst-su-commit`).
// ---------------------------------------------------------------------------

/// The changeover every case below opens a unit at — well clear of both of
/// `inst-su-instant`'s floors against this suite's fixed clock.
fn changeover() -> OffsetDateTime {
    at(10) + time::Duration::days(30)
}

/// The world a supersession is composed against: a plan whose revision is **current**.
///
/// `seed` leaves the revision `draft`, which is right for every other case in this
/// suite and wrong for this one by definition: a supersession supersedes a *published*
/// row, so its plan has published at least once and `plan_repo::load_current` — which
/// `submit_supersession_on` and `submit_window_mutation_on` both resolve their subject
/// through — has something to answer with. The flip uses this suite's own stand-in for
/// the publish commit, for that helper's stated reason.
async fn seed_published(h: &Harness) -> Seeded {
    let seeded = seed(h).await;
    publish_revision_at_the_table(h, seeded.revision).await;
    seeded
}

/// The subject string a record was opened under, read back from the store.
async fn record_subject(h: &Harness, approval_id: Uuid) -> String {
    let conn = h.provider.conn().expect("conn");
    bss_pricing::infra::storage::repo::approval_repo::read(&conn, &h.scope, TENANT, approval_id)
        .await
        .expect("read the unit")
        .expect("the unit is there")
        .subject_ref
}

async fn submit_supersession(
    h: &Harness,
    approval_id: Uuid,
    market: &str,
    at_instant: OffsetDateTime,
) -> Result<ApprovalRecord, DomainError> {
    let conn = h.provider.conn().expect("conn");
    ApprovalService::submit_supersession_on(
        &conn,
        &h.scope,
        TENANT,
        &scope_key(market),
        at_instant,
        approval_id,
        json!({ "reason": "thresholdReached" }),
        stamp(),
    )
    .await
}

#[tokio::test]
async fn a_supersession_opens_under_price_unit_and_holds_its_key() {
    // `price_unit` and not a token of its own: S5 §6's enumeration has no
    // `supersession` member, and this gear does not mint tokens the design set has not
    // declared. The change set is one price row on one canonical scope key, which is
    // what `price_unit` names — so the *subject string* is what carries the act.
    let h = harness().await;
    seed_published(&h).await;

    let record = submit_supersession(&h, Uuid::from_u128(0xa_5001), "eu", changeover())
        .await
        .expect("a supersession opens a unit");

    assert_eq!(record.subject_kind, AuditSubjectKind::PriceUnit);
    assert!(
        record.subject_ref.contains("supersession"),
        "the act is in the subject, since the token cannot carry it: {}",
        record.subject_ref
    );
    assert_eq!(record.state, ApprovalState::Submitted);
    // And the key it holds, which the name claims. One, unlike the cutover's two:
    // a supersession mints no generation, so pending the market key is the whole
    // of `inst-co-single-pending` here.
    // `a_second_submit_of_one_supersession_is_refused_by_the_subject_guard_specifically`
    // and `two_changeovers_on_one_key_are_two_acts_but_the_key_admits_only_one_unit`
    // assert what the next submit is *told*; this asserts what the register was
    // left holding, and a door that refused the second submit off a stale cache
    // would satisfy them and not this.

    assert_eq!(
        held_keys_of(&h, record.approval_id).await,
        vec![scope_key("eu").to_string()]
    );
}

#[tokio::test]
async fn a_second_submit_of_one_supersession_is_refused_by_the_subject_guard_specifically() {
    // The caller's own duplicate submit, answered before anybody else's work on the key.
    //
    // **Three guards answer this condition with one code, and a probe caught the first
    // version of this case not telling them apart** (2026-08-05). A duplicate *subject*
    // implies a duplicate *key*, so with the subject check disabled `refuse_held_key`
    // answers, and with that disabled too the held-key index answers — all three
    // `PENDING_CHANGE_UNIT_EXISTS`. So the assertion is on the **wording**, which is the
    // only thing that differs: only the subject guard says "this supersession of … at
    // <changeover>", because only it knows which act was duplicated. That is the same
    // arrangement D-192 records for its own guard-above-index pair: the check's proof is
    // the sentence it names the holder with, and the index's proof is one layer down.
    let h = harness().await;
    seed_published(&h).await;
    let first = Uuid::from_u128(0xa_5001);
    submit_supersession(&h, first, "eu", changeover())
        .await
        .expect("the first submit");

    let err = submit_supersession(&h, Uuid::from_u128(0xa_5002), "eu", changeover())
        .await
        .expect_err("one act, one pending unit");

    let DomainError::PendingChangeUnitExists(detail) = err else {
        panic!("got: {err:?}");
    };
    assert!(
        detail.starts_with("this supersession of"),
        "the subject guard's own sentence, which neither sibling produces: {detail}"
    );
    assert!(
        detail.contains(&format_rfc3339(changeover())),
        "and it names the act, not just the key: {detail}"
    );
    assert!(
        detail.contains(&first.to_string()),
        "the record holding it: {detail}"
    );
    assert!(detail.contains("withdraw"), "and the remedy: {detail}");
}

#[tokio::test]
async fn two_changeovers_on_one_key_are_two_acts_but_the_key_admits_only_one_unit() {
    // Two things at once, and they pull in opposite directions. The **subject** differs,
    // so the second submit is not the first one's duplicate — `supersession_unit_ref`
    // carries the changeover precisely so an approval of one cannot authorize the other.
    // But `inst-co-single-pending` is about the **key**, not the act: at most one pending
    // unit of any kind may hold it. So the second is still refused, by the *key* holder
    // rather than by the subject holder — a different sentence for a different reason,
    // and the sentence is what this asserts.
    //
    // It does **not** distinguish `refuse_held_key`'s check from the held-key index
    // beneath it: a probe showed both answer here with the same code and near-identical
    // wording, which is D-192's guard-above-index shape again. Telling those two apart is
    // `refuse_held_key`'s own business and is pinned in `storage_tests`; what this case
    // owns is that the **key** rule answers rather than the subject rule.
    let h = harness().await;
    seed_published(&h).await;
    submit_supersession(&h, Uuid::from_u128(0xa_5001), "eu", changeover())
        .await
        .expect("the first act");

    let err = submit_supersession(
        &h,
        Uuid::from_u128(0xa_5002),
        "eu",
        changeover() + time::Duration::days(1),
    )
    .await
    .expect_err("the key already has a pending unit");

    let DomainError::PendingChangeUnitExists(detail) = err else {
        panic!("got: {err:?}");
    };
    assert!(
        detail.starts_with("scope key"),
        "the key rule answered, not the subject rule: {detail}"
    );
    assert!(
        !detail.starts_with("this supersession of"),
        "and specifically not the subject guard, whose subject differs here: {detail}"
    );
}

#[tokio::test]
async fn a_supersession_on_another_key_of_one_plan_opens_its_own_unit() {
    // The other side of the rule: `inst-co-single-pending` holds a *key*, so two
    // supersessions on two keys of one plan are two units. Without this the previous
    // case's refusal would be indistinguishable from a per-plan lock, which is not what
    // the instruction says.
    let h = harness().await;
    seed_published(&h).await;
    submit_supersession(&h, Uuid::from_u128(0xa_5001), "eu", changeover())
        .await
        .expect("the first key");

    let second = submit_supersession(&h, Uuid::from_u128(0xa_5002), "us", changeover())
        .await
        .expect("a second key of the same plan is a second unit");

    assert_eq!(second.state, ApprovalState::Submitted);
    assert_ne!(
        second.subject_ref,
        record_subject(&h, Uuid::from_u128(0xa_5001)).await,
        "two keys, two acts, two subjects"
    );
}

#[tokio::test]
async fn the_supersession_unit_pins_the_plan_shape_a_reviewer_is_shown() {
    // The content pin is the plan shape, exactly as a window unit's is and for D-99's
    // reason applied one plane over: what a reviewer of a supersession is shown is the
    // plan whose row and intervals move, and `authorizing_unit` re-derives that shape at
    // commit — so an edit to any of the plan's rows between the approve and the commit
    // re-opens the review rather than riding the old signature.
    let h = harness().await;
    let seeded = seed_published(&h).await;
    let record = submit_supersession(&h, Uuid::from_u128(0xa_5001), "eu", changeover())
        .await
        .expect("open");

    assert!(
        !record.content_hash.is_empty(),
        "a unit with no pin would authorize any later content"
    );

    // Move a row of the plan and the pin no longer matches what the world says.
    let mut moved = flat_row();
    moved.row.amount_minor = Some(MinorAmount::new(2_500).expect("non-negative"));
    h.prices
        .update_draft(
            &h.scope,
            TENANT,
            seeded.price_id,
            seeded.price_version,
            moved,
            stamp(),
            /* on_behalf_of */ None,
        )
        .await
        .expect("edit the plan's row");

    let after = submit_supersession(&h, Uuid::from_u128(0xa_5002), "us", changeover())
        .await
        .expect("a unit on another key, opened against the moved shape");
    assert_ne!(
        after.content_hash, record.content_hash,
        "the pin follows the plan's content, so an edit between two submits is visible"
    );
}

// ---------------------------------------------------------------------------
// The cutover's unit: two keys, one act (`inst-co-single-pending`, D-100)
// ---------------------------------------------------------------------------

async fn submit_cutover(
    h: &Harness,
    approval_id: Uuid,
    market: &str,
    at_instant: OffsetDateTime,
) -> Result<ApprovalRecord, DomainError> {
    let conn = h.provider.conn().expect("conn");
    ApprovalService::submit_cutover_on(
        &conn,
        &h.scope,
        TENANT,
        &[scope_key(market)],
        at_instant,
        approval_id,
        json!({ "reason": "alwaysMaterialTrigger" }),
        stamp(),
    )
    .await
}

/// The keys one record holds, read back from the register.
async fn held_keys_of(h: &Harness, approval_id: Uuid) -> Vec<String> {
    let conn = h.provider.conn().expect("conn");
    bss_pricing::infra::storage::repo::approval_repo::held_keys_of(
        &conn,
        &h.scope,
        TENANT,
        approval_id,
    )
    .await
    .expect("read the register")
}

#[tokio::test]
async fn a_cutover_holds_the_market_key_and_the_generation_it_mints() {
    // **Two keys, and the second is the whole difference from a supersession.**
    // `inst-co-single-pending` pends *"the `all_subscriptions` key and the **new
    // generation's** key — prior generations are not pended"*. Without the first,
    // a window mutation could be approved against the key mid-cutover; without the
    // second, two cutovers of one plan at one instant would each read the
    // generation as free, and the loser would meet a partial-`UNIQUE` violation
    // inside its commit instead of a refusal at submit.
    let h = harness().await;
    seed_published(&h).await;

    let record = submit_cutover(&h, Uuid::from_u128(0xa_6001), "eu", changeover())
        .await
        .expect("a cutover opens a unit");

    assert_eq!(record.subject_kind, AuditSubjectKind::PriceUnit);
    assert!(
        record.subject_ref.contains("/cutover/"),
        "the act is in the subject, since S5 section 6 declares no cutover token: {}",
        record.subject_ref
    );
    let held = held_keys_of(&h, record.approval_id).await;
    assert_eq!(held.len(), 2, "the market key and the generation: {held:?}");
    assert!(
        held.iter().any(|key| key == &scope_key("eu").to_string()),
        "the all_subscriptions key is pended: {held:?}"
    );
    assert!(
        held.iter()
            .any(|key| key.contains("existing_grandfathered")),
        "and the generation this act mints: {held:?}"
    );
}

#[tokio::test]
async fn another_act_cannot_be_submitted_against_a_key_a_cutover_holds() {
    // The **first** key's purpose, asserted through the thing it exists to stop
    // rather than by reading the register: mid-cutover the market key is spoken
    // for, so another unit over it is refused at submit instead of racing the
    // commit. A supersession is the act used here because it is the other producer
    // of `published -> superseded` on that very key.
    let h = harness().await;
    seed_published(&h).await;
    submit_cutover(&h, Uuid::from_u128(0xa_6001), "eu", changeover())
        .await
        .expect("the cutover opens");

    let err = submit_supersession(&h, Uuid::from_u128(0xa_6002), "eu", changeover())
        .await
        .expect_err("the key is held by the cutover");

    let DomainError::PendingChangeUnitExists(message) = &err else {
        panic!("got: {err:?}");
    };
    // **This case cannot separate the two guards that answer here, and that is
    // stated rather than papered over** — the third instance of D-192's
    // guard-above-index shape in this crate. `refuse_held_key` is the explanatory
    // check; the `pricing_approval_key` index is the guarantee one layer down, and
    // `infra::storage`'s fold maps both to `PENDING_CHANGE_UNIT_EXISTS`. Deleting
    // the check leaves this case green — **measured, not assumed** — and so does
    // asserting the message names the holding act, because the index's body names
    // the subject too. `infra::storage`'s own comment predicts exactly that: both
    // name the key and both end in the same instruction, and only *whether a unit
    // is named* differs, which the winning transaction's commit timing decides.
    //
    // So what this case pins is the **behaviour an operator meets**: the key is
    // spoken for, the answer says which act holds it, and the remedy is in the
    // sentence. Separating the pair needs a race the store's own suite stages
    // (`storage_tests`), not a service-level submit.
    assert!(
        message.contains("/cutover/"),
        "the refusal names the act holding the key, whichever guard answered: {message}"
    );
}

#[tokio::test]
async fn a_widened_selection_is_a_new_act_and_collides_on_the_keys_it_shares() {
    // The **second** key's purpose, and the shape that reaches it.
    //
    // My first version of this case asserted that two cutovers of one plan at one
    // instant collide on the generation, and it was **false**: the generation key
    // carries every axis but `priceEligibility` and `cohort`, the market included,
    // so two markets mint two generations and nothing collides. That is
    // `grandfathered_copy_key` behaving exactly as `inst-co-copy` says.
    //
    // What does reach it is D-28's own scenario: a second submit whose **selection
    // differs**, which is a different key-set hash and therefore a different
    // subject, so the subject guard lets it past — and then the keys the two
    // selections share are already pended. Both halves matter: this is the guard
    // that stops one plan being cut over twice through two differently-named acts.
    let h = harness().await;
    seed_published(&h).await;
    submit_cutover(&h, Uuid::from_u128(0xa_6001), "eu", changeover())
        .await
        .expect("the first cutover opens");

    let conn = h.provider.conn().expect("conn");
    let err = ApprovalService::submit_cutover_on(
        &conn,
        &h.scope,
        TENANT,
        &[scope_key("eu"), scope_key("us")],
        changeover(),
        Uuid::from_u128(0xa_6002),
        json!({ "reason": "alwaysMaterialTrigger" }),
        stamp(),
    )
    .await
    .expect_err("the widened selection shares eu's two keys with the standing unit");

    assert!(
        matches!(err, DomainError::PendingChangeUnitExists(_)),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn two_markets_of_one_plan_cut_over_independently() {
    // The control for the case above, and it is what keeps the held-key set from
    // being read as "one cutover per plan". Two markets are two generations, and
    // nothing about one act touches the other's keys.
    let h = harness().await;
    seed_published(&h).await;
    submit_cutover(&h, Uuid::from_u128(0xa_6001), "eu", changeover())
        .await
        .expect("the eu cutover opens");

    submit_cutover(&h, Uuid::from_u128(0xa_6002), "us", changeover())
        .await
        .expect("a different market is a different set of keys");
}

#[tokio::test]
async fn a_selection_spanning_two_plans_is_refused() {
    // The subject's first segment is the plan id — `subject_aggregate` refuses a
    // subject that is not `<plan_id>/<rest>` — so a selection over two plans has
    // no segment to be named by. One refusal here, naming both plans, rather than
    // a corrupt subject the register rejects three layers down.
    let h = harness().await;
    seed_published(&h).await;

    let other_plan = ScopeKey::new(
        PlanId::new(Uuid::from_u128(0x9_9999)),
        CurrencyCode::new("EUR").expect("three letters"),
        Region::new("eu").expect("a non-blank region"),
        terminal_phase(),
        PriceEligibility::AllSubscriptions,
        ChargeKind::Recurring,
        Cohort::None,
    )
    .expect("all_subscriptions pairs with cohort none");

    let conn = h.provider.conn().expect("conn");
    let err = ApprovalService::submit_cutover_on(
        &conn,
        &h.scope,
        TENANT,
        &[scope_key("eu"), other_plan],
        changeover(),
        Uuid::from_u128(0xa_6003),
        json!({ "reason": "alwaysMaterialTrigger" }),
        stamp(),
    )
    .await
    .expect_err("one act cuts over one plan");

    assert!(
        matches!(err, DomainError::InvalidRequest(_)),
        "got: {err:?}"
    );
}
