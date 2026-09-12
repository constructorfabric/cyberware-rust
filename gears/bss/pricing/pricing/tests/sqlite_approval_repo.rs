//! The approval repository against the executed `SQLite` mirror.
//!
//! Three operations and one gate. `open` writes a pending record, `read` finds
//! it under the tenant that owns it and **only** that tenant, and `decide`
//! performs the one flip §4 sanctions. What makes this a database suite rather
//! than a unit test is the second half of each: the record read back is the row
//! the store actually holds, the foreign scope is refused in SQL rather than by
//! a `WHERE` clause this file wrote, and the refusals are the ones the trigger
//! and the CHECKs produce.
//!
//! **Every refusal test first puts the world in the state where the object under
//! test is what answers.** A decision refused because the record was never
//! written is not evidence about `inst-as-immutable`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use async_trait::async_trait;
use bss_pricing::domain::approval::{ApprovalDecision, ApprovalState};
use bss_pricing::domain::audit::{AuditStamp, AuditSubjectKind};
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::materiality::triggers::Trigger;
use bss_pricing::domain::materiality::{self, ChangeSet};
use bss_pricing::domain::membership_change::{MembershipMoveProposal, MembershipMoveSet};
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::approval::{ApprovalService, PinnedSubject};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::approval_repo::{
    self, ApprovalRecord, NewApproval, SubjectAggregate,
};
use bss_pricing::infra::storage::repo::group_membership_repo;
use bss_pricing_sdk::catalog_version::CatalogVersion;
use bss_pricing_sdk::catalog_version_registry::{CatalogVersionRegistryV1, PendingVersionRef};
use time::OffsetDateTime;

use sea_orm_migration::MigratorTrait;
use serde_json::json;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, SecureInsertExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const OTHER_TENANT: Uuid = Uuid::from_u128(0x7e_22);
const SUBMITTER: Uuid = Uuid::from_u128(0x5b_01);
const APPROVER: Uuid = Uuid::from_u128(0xab_01);

/// One value for the whole binary: this suite drives the repository directly,
/// where the value the HTTP edge would have established has no producer.
const TEST_CORRELATION: Uuid = Uuid::from_u128(0x_c0_11_a7_10);

/// The stamp an act is written under: who, when, and the request's correlation.
fn stamp_of(actor: Uuid, when: OffsetDateTime) -> AuditStamp {
    AuditStamp {
        actor_principal_id: actor,
        recorded_at: when,
        correlation_id: TEST_CORRELATION,
    }
}

async fn harness() -> DBProvider<DbError> {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .expect("run migrator");
    DBProvider::<DbError>::new(db)
}

fn at(hour: u32) -> OffsetDateTime {
    utc_ymd_hms(2026, 8, 3, hour, 0, 0)
}

fn pending(approval_id: Uuid, tenant_id: Uuid) -> NewApproval {
    NewApproval {
        approval_id,
        tenant_id,
        subject_ref: "0000ffff-0000-0000-0000-000000000001/3".to_owned(),
        subject_kind: AuditSubjectKind::PlanRevision,
        content_hash: vec![0xde, 0xad, 0xbe, 0xef],
        materiality: json!({ "reason": "noConfiguredThreshold" }),
        // **No key**, and deliberately so: this file's subject is the record's own
        // state machine, and several of its cases open one unit per tenant over the
        // same synthetic subject. A key here would make
        // `uq_pricing_approval_key_pending` the thing that refused them, and every
        // case below would be about the register instead of about the plane it
        // names. The register has its own suite (`tests/sqlite_window_service.rs`)
        // and its own race (`tests/postgres_approval_race.rs`).
        held_keys: std::collections::BTreeSet::new(),
    }
}

/// A pending unit over a **named** subject — `pending`'s variant for the cases
/// that need more than one plan, or a kind other than `plan_revision`.
fn pending_over(
    approval_id: Uuid,
    tenant_id: Uuid,
    subject_ref: &str,
    subject_kind: AuditSubjectKind,
) -> NewApproval {
    NewApproval {
        subject_ref: subject_ref.to_owned(),
        subject_kind,
        ..pending(approval_id, tenant_id)
    }
}

/// The stamp every `pending(..)` submission in this file is opened under.
///
/// The submitter and the submission instant live on the stamp rather than on
/// [`NewApproval`], so they are named once here and asserted against below.
fn opened_under() -> AuditStamp {
    stamp_of(SUBMITTER, at(9))
}

/// The whole record the store must hold once `new` has been decided.
///
/// Built from the **submission** and from the decision the caller asked for —
/// never from the row or the return value being asserted about. An expectation
/// read out of the thing under test asserts only that a value equals itself:
/// this file used to compare `decided.content_hash` against `opened.content_hash`
/// where both came from one pre-decision read, so `reason` and `decided_at` had
/// no coverage at all and moving the stored `decided_at` by five hours left the
/// suite green.
fn decided_from(
    new: &NewApproval,
    state: ApprovalState,
    approver_principal: Option<Uuid>,
    reason: Option<&str>,
    decided_at: OffsetDateTime,
) -> ApprovalRecord {
    ApprovalRecord {
        approval_id: new.approval_id,
        tenant_id: new.tenant_id,
        subject_ref: new.subject_ref.clone(),
        subject_kind: new.subject_kind,
        content_hash: new.content_hash.clone(),
        state,
        submitter_principal: SUBMITTER,
        approver_principal,
        reason: reason.map(ToOwned::to_owned),
        materiality: new.materiality.clone(),
        submitted_at: at(9),
        decided_at: Some(decided_at),
    }
}

#[tokio::test]
async fn an_opened_record_is_pending_and_reads_back_as_it_was_written() {
    // The world in which every refusal below is observable. Without it a suite
    // of refusals would pass against a repository that writes nothing at all.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xa1);

    let opened = approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect("open the record");

    assert_eq!(opened.state, ApprovalState::Submitted);
    assert_eq!(opened.approver_principal, None);
    assert_eq!(opened.reason, None);
    assert_eq!(opened.decided_at, None);

    let read = approval_repo::read(&conn, &scope, TENANT, id)
        .await
        .expect("read the record")
        .expect("the record is there");
    assert_eq!(read, opened, "what was read is what was written");
    assert_eq!(read.subject_kind, AuditSubjectKind::PlanRevision);
    assert_eq!(read.content_hash, vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(read.submitted_at, at(9));
}

#[tokio::test]
async fn a_foreign_tenant_sees_absence_rather_than_a_refusal() {
    // The catalog is commercially sensitive: "forbidden" would confirm the
    // record exists. The scope is applied in SQL, so this holds without the
    // caller passing a filter.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0xa2);
    approval_repo::open(
        &conn,
        &AccessScope::for_tenant(TENANT),
        pending(id, TENANT),
        opened_under(),
    )
    .await
    .expect("open the record");

    let foreign = approval_repo::read(&conn, &AccessScope::for_tenant(OTHER_TENANT), TENANT, id)
        .await
        .expect("a foreign read is not an error");
    assert!(foreign.is_none(), "a foreign tenant sees nothing");
}

#[tokio::test]
async fn a_record_that_was_never_opened_reads_as_absent() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let absent = approval_repo::read(
        &conn,
        &AccessScope::for_tenant(TENANT),
        TENANT,
        Uuid::from_u128(0xdead),
    )
    .await
    .expect("an absent record is not an error");
    assert!(absent.is_none());
}

#[tokio::test]
async fn a_pending_record_is_approved_and_the_decision_columns_are_the_ones_that_moved() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xa3);
    let submission = pending(id, TENANT);
    approval_repo::open(&conn, &scope, submission.clone(), opened_under())
        .await
        .expect("open");

    let decided = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("approve the record");

    // The row the store holds, against the record the submission and the
    // decision entail. Four columns moved and eight did not — `content_hash`
    // above all, which **is** the TOCTOU pin, so a decision that could re-pin it
    // would launder the mutation the guard exists to catch.
    let stored = approval_repo::read(&conn, &scope, TENANT, id)
        .await
        .expect("read the decided record")
        .expect("the record is there");
    assert_eq!(
        stored,
        decided_from(
            &submission,
            ApprovalState::Approved,
            Some(APPROVER),
            None,
            at(11)
        )
    );

    // And what `decide` handed back is that row rather than a reconstruction of
    // it. The function returns the pre-decision read with four fields
    // substituted, so nothing but this comparison stands between the caller's
    // copy and the store's.
    assert_eq!(decided, stored);
}

#[tokio::test]
async fn a_rejection_carries_its_reason_and_a_withdraw_names_no_approver() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    // `inst-as-reject`'s mandatory reason, read back out of the column that
    // holds it rather than off the value the caller passed in.
    let rejected = Uuid::from_u128(0xa4);
    let rejected_submission = pending(rejected, TENANT);
    approval_repo::open(&conn, &scope, rejected_submission.clone(), opened_under())
        .await
        .expect("open");
    let record = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        rejected,
        ApprovalDecision::Reject,
        Some(APPROVER),
        Some("margin below floor".to_owned()),
        stamp_of(APPROVER, at(12)),
    )
    .await
    .expect("reject the record");
    let stored = approval_repo::read(&conn, &scope, TENANT, rejected)
        .await
        .expect("read the rejected record")
        .expect("the record is there");
    assert_eq!(
        stored,
        decided_from(
            &rejected_submission,
            ApprovalState::Rejected,
            Some(APPROVER),
            Some("margin below floor"),
            at(12)
        )
    );
    assert_eq!(record, stored);

    // `inst-as-void`: a withdraw has no approver at all, and the store agrees —
    // `chk_pricing_approval_approver` exempts `voided` precisely because the
    // withdrawer is the submitter, whom the distinctness rule bars from that
    // column.
    let voided = Uuid::from_u128(0xa5);
    let voided_submission = pending(voided, TENANT);
    approval_repo::open(&conn, &scope, voided_submission.clone(), opened_under())
        .await
        .expect("open");
    let record = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        voided,
        ApprovalDecision::Void,
        None,
        Some("withdrawn by the submitter".to_owned()),
        stamp_of(SUBMITTER, at(13)),
    )
    .await
    .expect("withdraw the record");
    let stored = approval_repo::read(&conn, &scope, TENANT, voided)
        .await
        .expect("read the voided record")
        .expect("the record is there");
    assert_eq!(
        stored,
        decided_from(
            &voided_submission,
            ApprovalState::Voided,
            None,
            Some("withdrawn by the submitter"),
            at(13)
        )
    );
    assert_eq!(record, stored);
}

#[tokio::test]
async fn a_second_decision_on_a_decided_record_is_refused_as_not_pending() {
    // `inst-as-immutable`, through the repository rather than through the
    // trigger: the caller is told `APPROVAL_NOT_PENDING` (409) instead of being
    // handed a driver error, which is a 500 telling an operator to page someone
    // about a race a client can simply retry-free re-read.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xa6);
    approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect("open");
    approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("the first decision lands");

    let err = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Reject,
        Some(APPROVER),
        Some("changed my mind".to_owned()),
        stamp_of(APPROVER, at(12)),
    )
    .await
    .expect_err("a decided record takes no second decision");

    match err {
        RepoError::ApprovalNotPending { state, .. } => assert_eq!(state, "approved"),
        other => panic!("expected the pendingness refusal, got: {other:?}"),
    }
}

#[tokio::test]
async fn deciding_a_record_that_does_not_exist_is_absence_and_not_a_storage_fault() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let err = approval_repo::decide(
        &conn,
        &AccessScope::for_tenant(TENANT),
        TENANT,
        Uuid::from_u128(0xbeef),
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect_err("there is nothing to decide");
    assert!(matches!(err, RepoError::NotFound { .. }), "got: {err:?}");
}

#[tokio::test]
async fn a_decision_by_a_foreign_tenant_finds_nothing_to_decide() {
    // What answers here is `decide`'s **read**, and the comment this replaces
    // claimed otherwise — it said "the BOLA case on the write path", and the
    // write path's scope could be dropped entirely with this test and the whole
    // suite green. It is worth keeping as what it actually is: a foreign caller
    // is told absence, from the first statement `decide` issues, and the record
    // is untouched. The `UPDATE`'s own gate is a different object and cannot be
    // reached through this function while the read stands in front of it;
    // `approval_repo_tests::a_foreign_scope_writes_nothing_on_the_decision_path`
    // executes it directly.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let id = Uuid::from_u128(0xa7);
    approval_repo::open(
        &conn,
        &AccessScope::for_tenant(TENANT),
        pending(id, TENANT),
        opened_under(),
    )
    .await
    .expect("open");

    let err = approval_repo::decide(
        &conn,
        &AccessScope::for_tenant(OTHER_TENANT),
        TENANT,
        id,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect_err("a foreign scope decides nothing");
    assert!(matches!(err, RepoError::NotFound { .. }), "got: {err:?}");

    // And the record is untouched.
    let still = approval_repo::read(&conn, &AccessScope::for_tenant(TENANT), TENANT, id)
        .await
        .expect("read")
        .expect("still there");
    assert_eq!(still.state, ApprovalState::Submitted);
}

#[tokio::test]
async fn re_opening_a_record_under_an_id_that_is_taken_is_contention_and_not_a_storage_fault() {
    // D-159's reading applied to this table's one serialization point, the
    // primary key. `CONCURRENT_MUTATION` (409) is what a retry can act on; a 500
    // is what pages an operator.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xa8);
    approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect("open");

    let err = approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect_err("the id is taken");
    match err {
        RepoError::ConcurrentMutation { aggregate } => {
            assert!(aggregate.contains(&id.to_string()), "got: {aggregate}");
        }
        other => panic!("expected contention, got: {other:?}"),
    }
}

/// A self-approval is **still** unstorable, beneath the surface that now
/// refuses it.
///
/// **This is the inversion of what this test recorded when it was written**, and
/// the sentence it replaces was: "the two-person rule's *surface* refusal —
/// `SELF_APPROVAL_FORBIDDEN` (403) with its audited attempted-violation record —
/// is G3's". It is no longer owed:
/// `infra::approval::ApprovalService::decide` answers it, over
/// `domain::approval::authorize_decision`, and
/// `tests/sqlite_approval_service.rs::a_self_approval_is_refused_and_the_attempt_is_recorded`
/// is where the 403 **and** its `deny` record are asserted.
///
/// The test stays because what it asserts stayed true and is now the *second*
/// line rather than the only one: this repository is reachable without the
/// service — the service is the only caller today, and nothing in the type
/// system says so — and the CHECK is what a caller that went round it meets. A
/// deleted test here would have made the constraint's removal invisible.
#[tokio::test]
async fn a_self_approval_row_is_still_refused_by_the_store_beneath_the_surface() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xa9);
    approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect("open");

    let err = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Approve,
        Some(SUBMITTER),
        None,
        stamp_of(SUBMITTER, at(11)),
    )
    .await
    .expect_err("the submitter may not approve their own record");
    let message = err.to_string();
    assert!(
        message.contains("chk_pricing_approval_distinct_principals"),
        "the refusal must name the constraint it came from, got: {message}"
    );
}

/// A reason-less reject is **still** unstorable, and the code is no longer owed.
///
/// **This is the inversion this test asked for.** It used to close with "when G3
/// lands, this test must be inverted rather than deleted", because
/// `REASON_REQUIRED` was declared in `design/05-governance.md` §5 and had no
/// rendering: a reason-less reject reached this CHECK and surfaced as
/// `RepoError::Db` — a 500 for a request an author could have fixed by typing a
/// sentence.
///
/// It has one now.
/// `domain::approval::decision::REASON_REQUIRED` is the code,
/// `DomainError::ReasonRequired` renders it, and
/// `tests/sqlite_approval_service.rs::a_reject_with_no_reason_is_refused_with_the_code_and_not_as_a_storage_fault`
/// asserts that this constraint is no longer what answers — by name.
///
/// What is *not* inverted is the executed refusal below, and deliberately: the
/// surface refusing first does not make the store's CHECK redundant, it makes it
/// the thing a caller that bypassed the surface meets. Deleting it would remove
/// the only executable evidence that `chk_pricing_approval_reason` still exists.
#[tokio::test]
async fn a_reject_without_a_reason_is_still_refused_by_the_check_beneath_the_code() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let id = Uuid::from_u128(0xaa);
    approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
        .await
        .expect("open");

    let err = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        id,
        ApprovalDecision::Reject,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect_err("a reject needs a reason");
    let message = err.to_string();
    assert!(
        message.contains("chk_pricing_approval_reason"),
        "the refusal must name the constraint it came from, got: {message}"
    );
}

/// # Why the row goes in past `open`
///
/// The property is the **mirror's**: that
/// `chk_pricing_approval_subject_kind` admits every member of
/// `AuditSubjectKind::ALL` and that `to_domain` reads each back as its typed form.
/// `open` additionally resolves the unit's *aggregate* to append the trail, and
/// since 2026-08-04 that resolution is per kind — a `window` unit's plan comes off
/// the window's canonical scope key, so `open` needs a **real window** to write one.
///
/// Staging a `window` unit here with a plan-revision `subject_ref`, which is what
/// this test used to do, is staging a record this crate cannot write: it passed only
/// because the aggregate resolution was one parse for every kind, and the parse
/// happened to accept a ref no window unit carries. So the insert is direct, which is
/// what the test is named for, and `open`'s per-kind behaviour is asserted where a
/// real window exists (`tests/sqlite_window_service.rs`).
#[tokio::test]
async fn every_subject_kind_d158_declares_is_storable_on_the_mirror() {
    // D-158 requires `pricing_approval` and `pricing_audit_log` to spell one
    // enumeration and to be **extended together**. A test naming today's three
    // tokens would go on passing the day a fourth is added to
    // `AuditSubjectKind` and not to `chk_pricing_approval_subject_kind` — which
    // is the drift the decision exists to prevent — so the range is `ALL`, and
    // the assertion is that the mirror the e2e and dev boot on can hold every
    // member of it.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    for (n, kind) in AuditSubjectKind::ALL.iter().enumerate() {
        let id = Uuid::from_u128(0xb0 + n as u128);
        let am = bss_pricing::infra::storage::entity::approval::ActiveModel {
            approval_id: sea_orm::ActiveValue::Set(id),
            tenant_id: sea_orm::ActiveValue::Set(TENANT),
            subject_ref: sea_orm::ActiveValue::Set(format!("subject-{n}")),
            subject_kind: sea_orm::ActiveValue::Set(kind.as_str().to_owned()),
            content_hash: sea_orm::ActiveValue::Set(vec![0xde, 0xad]),
            state: sea_orm::ActiveValue::Set("submitted".to_owned()),
            submitter_principal: sea_orm::ActiveValue::Set(SUBMITTER),
            approver_principal: sea_orm::ActiveValue::Set(None),
            reason: sea_orm::ActiveValue::Set(None),
            materiality: sea_orm::ActiveValue::Set(json!({})),
            submitted_at: sea_orm::ActiveValue::Set(at(12)),
            decided_at: sea_orm::ActiveValue::Set(None),
        };
        sea_orm::EntityTrait::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)
            .expect("scope the insert")
            .exec(&conn)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "chk_pricing_approval_subject_kind must admit {}: {e}",
                    kind.as_str()
                )
            });

        let stored = approval_repo::read(&conn, &scope, TENANT, id)
            .await
            .expect("read")
            .expect("the record is there");
        assert_eq!(stored.subject_kind, *kind);
    }
}

/// **And a token the enumeration does not declare is refused.**
///
/// The other half of the CHECK, and it was missing. Its sibling above asserts only
/// that every declared token is *admitted*, so deleting
/// `chk_pricing_approval_subject_kind` outright left it green — a guard whose
/// removal proof came back zero. D-158's rule is a **partition**: the store holds
/// exactly the tokens the enumeration declares, and a store that held any string at
/// all would let `subject_kind` drift into free text one careless writer at a time,
/// with `to_domain`'s `CorruptRow` (a 500) as the first anybody heard of it.
///
/// The statement only this guard can refuse: an `INSERT` whose `subject_kind` is
/// well formed in every other respect and is not in the list. No neighbour shadows
/// it — the row satisfies `chk_pricing_approval_state`, `_decided_at`, `_reason`,
/// `_approver` and `_distinct_principals` by construction, and the append-only
/// triggers are all `BEFORE UPDATE`/`BEFORE DELETE` except `_born_submitted`, which
/// this row satisfies by arriving `submitted`.
///
/// `overlay` is the probe deliberately: it is a token S5 §6 **declares** and this
/// gear has no writer for, so the case also states the rule the other way round —
/// a member of the design set's list is not storable here until it has a writer.
#[tokio::test]
async fn a_subject_kind_outside_the_enumeration_is_refused_by_the_mirror() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    // `overlay` stood at the head of this list until 2026-08-06 and moved out of it
    // rather than being deleted: D-221 gave the overlay plane an audit writer, and
    // D-158 obliged this mirror to admit what the audit vocabulary declares, so
    // `chk_pricing_approval_subject_kind` admits it. `membership` sits beside it, on
    // the reasoning that `AuditSubjectKind::Membership` (2026-08-11) has an
    // audit-plane writer and, deliberately, no approval-plane one, so this CHECK
    // — `pricing_approval`'s, not `pricing_audit_log`'s — need not admit it yet.
    // **That reasoning was wrong, and it cost this probe its own false
    // negative.** `pricing_price`'s migration doc states why:
    // `every_subject_kind_d158_declares_is_storable_on_the_mirror` (this file)
    // binds the *declaration*, not who currently writes, so a declared
    // `AuditSubjectKind` variant obliges this CHECK on the very next test run
    // regardless of whether an approval-plane writer exists — and
    // `chk_pricing_price_unit_rate_nano` widened it accordingly. This probe stayed on
    // `membership` through that widening and started asserting the opposite of
    // what the store does, the same failure mode `overlay` left behind here
    // once already.
    //
    // The replacement is synthetic — `not_a_subject_kind` — for exactly
    // `approval_repo_tests::a_subject_kind_outside_d158s_enumeration_is_a_corrupt_row`'s
    // reason (that sibling probe went through the identical fix, and rejected a
    // third real-word candidate, `historical_import`, before it could repeat the
    // pattern here too): this probe's whole claim is that an *undeclared* token
    // is refused, every real domain word here is a candidate to become declared
    // — and, as `membership` just showed, "no approval-plane writer" is no
    // defence against that — and a token with no referent anywhere in the design
    // set or the codebase cannot be declared out from under the test. The three
    // that follow it are the shapes a token can be malformed in (empty, wrong
    // case, trailing space) and are not going anywhere.
    for undeclared in ["not_a_subject_kind", "", "PLAN_REVISION", "plan_revision "] {
        let id = Uuid::now_v7();
        let am = bss_pricing::infra::storage::entity::approval::ActiveModel {
            approval_id: sea_orm::ActiveValue::Set(id),
            tenant_id: sea_orm::ActiveValue::Set(TENANT),
            subject_ref: sea_orm::ActiveValue::Set("subject".to_owned()),
            subject_kind: sea_orm::ActiveValue::Set(undeclared.to_owned()),
            content_hash: sea_orm::ActiveValue::Set(vec![0xde, 0xad]),
            state: sea_orm::ActiveValue::Set("submitted".to_owned()),
            submitter_principal: sea_orm::ActiveValue::Set(SUBMITTER),
            approver_principal: sea_orm::ActiveValue::Set(None),
            reason: sea_orm::ActiveValue::Set(None),
            materiality: sea_orm::ActiveValue::Set(json!({})),
            submitted_at: sea_orm::ActiveValue::Set(at(12)),
            decided_at: sea_orm::ActiveValue::Set(None),
        };
        let outcome = sea_orm::EntityTrait::insert(am.clone())
            .secure()
            .scope_with_model(&scope, &am)
            .expect("scope the insert")
            .exec(&conn)
            .await;
        let err = outcome.err().unwrap_or_else(|| {
            panic!("chk_pricing_approval_subject_kind must refuse {undeclared:?}")
        });
        assert!(
            format!("{err}").contains("chk_pricing_approval_subject_kind"),
            "and refuse it by name, so the refusal names the rule: {err}"
        );
    }
}

#[tokio::test]
async fn every_state_the_machine_declares_is_reached_by_a_decision_and_stored() {
    // The same argument one enumeration over: `chk_pricing_approval_state` lists
    // four tokens and `ApprovalState::ALL` names four, and a state added to one
    // side alone is a row the other cannot write or cannot read. The route is
    // the point too — each decided state is *reached* through the decision that
    // produces it, so a state the machine declares and no `ApprovalDecision`
    // lands on fails the final comparison rather than going unnoticed.
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    let mut reached = vec![ApprovalState::Submitted];
    for (n, decision) in ApprovalDecision::ALL.iter().enumerate() {
        let outcome = decision.outcome();
        let id = Uuid::from_u128(0xc0 + n as u128);
        approval_repo::open(&conn, &scope, pending(id, TENANT), opened_under())
            .await
            .expect("open");
        approval_repo::decide(
            &conn,
            &scope,
            TENANT,
            id,
            *decision,
            outcome.requires_approver().then_some(APPROVER),
            outcome
                .requires_reason()
                .then(|| "margin below floor".to_owned()),
            stamp_of(APPROVER, at(11)),
        )
        .await
        .unwrap_or_else(|e| panic!("the store must take a {outcome} decision: {e}"));

        let stored = approval_repo::read(&conn, &scope, TENANT, id)
            .await
            .expect("read")
            .expect("the record is there");
        assert_eq!(stored.state, outcome);
        reached.push(outcome);
    }

    reached.sort_unstable();
    reached.dedup();
    let mut declared = ApprovalState::ALL.to_vec();
    declared.sort_unstable();
    assert_eq!(
        reached, declared,
        "every state the machine declares must be one the store was driven into"
    );
}

// ---------------------------------------------------------------------------
// The mint guard: one open policy proposal per tenant, as an index (D-192)
// ---------------------------------------------------------------------------

/// One `NewApproval` over a threshold-policy version, the shape
/// `infra::approval::open_policy_unit` builds.
///
/// `subject_ref` is the **version number** and nothing else — a policy unit has no
/// plan and no revision — which is why the rule this file is about cannot be the
/// exact-`subject_ref` reading the other planes use: every proposal names a different
/// subject. It holds no key, for `open_policy_unit`'s own reason: a threshold policy
/// prices nothing, so freezing a canonical scope key while a reviewer thinks about it
/// would make every publish in the tenant wait on a governance edit.
fn policy_proposal(approval_id: Uuid, tenant_id: Uuid, version: u64) -> NewApproval {
    NewApproval {
        approval_id,
        tenant_id,
        subject_ref: version.to_string(),
        subject_kind: AuditSubjectKind::Policy,
        content_hash: vec![0xc0, 0x1d],
        materiality: json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
        held_keys: std::collections::BTreeSet::new(),
    }
}

/// **The case that isolates `uq_pricing_approval_policy_pending` from the check in
/// front of it (D-192 clause (2)).**
///
/// `infra::approval::open_policy_unit` reads `find_pending_policy_unit` and *then*
/// inserts, so every path through the service refuses a second proposal before this
/// index is consulted — which means a service-level or route-level case stays green
/// with the index dropped and proves nothing about it. The two writers that race are
/// two calls to `approval_repo::open`, and that is exactly what this drives: the
/// check is bypassed rather than mocked, so what answers here can only be the store.
///
/// What the index prevents is the **mint**, not the second unit for its own sake. Two
/// proposals that both get past the check mint version *n* off the same
/// `threshold_repo::latest_version` and both insert their entries under the key
/// `(tenant, version, currency)` — which permits disjoint currency sets — leaving one
/// version number holding a row set no approver ever saw, on a table that refuses
/// `UPDATE` and `DELETE`.
///
/// And the answer is asserted, not only the refusal: `PendingPolicyUnitHeld` rather
/// than `ConcurrentMutation`, because the two indexes on this table want different
/// things said to their losers and `policy_guard_or_contention` is the only place
/// that can tell them apart.
#[tokio::test]
async fn a_second_open_policy_proposal_is_refused_by_the_mint_guard_and_not_as_contention() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    let first = Uuid::from_u128(0xb1);
    approval_repo::open(
        &conn,
        &scope,
        policy_proposal(first, TENANT, 0),
        opened_under(),
    )
    .await
    .expect("the first proposal opens its unit");

    let refused = approval_repo::open(
        &conn,
        &scope,
        // A **different** id and a **different** version number, which is the pair the
        // corrupt state is actually reached through: nothing here collides on a
        // primary key, and an index keyed on the version number would have admitted
        // it.
        policy_proposal(Uuid::from_u128(0xb2), TENANT, 1),
        stamp_of(SUBMITTER, at(10)),
    )
    .await
    .expect_err("the index refuses a second open proposal for one tenant");

    assert_eq!(
        refused,
        RepoError::PendingPolicyUnitHeld {
            tenant_id: TENANT.to_string()
        },
        "the mint guard's loser is a pending-unit conflict, not a retry"
    );

    // **And nothing landed.** A refusal that had written the row first would leave the
    // tenant with two units and the guard reporting a violation it had already
    // permitted.
    assert!(
        approval_repo::read(&conn, &scope, TENANT, Uuid::from_u128(0xb2))
            .await
            .expect("the read succeeds")
            .is_none(),
        "the refused proposal opened no second unit"
    );
    assert_eq!(
        approval_repo::find_pending_policy_unit(&conn, &scope, TENANT)
            .await
            .expect("the pending read succeeds")
            .map(|held| held.approval_id),
        Some(first),
        "and the unit still under review is the first one"
    );
}

/// The guard is **partial**, so the tenant is not locked out for ever.
///
/// Paired with the case above for `a_withdrawn_proposal_frees_the_tenant_to_propose_again`'s
/// reason, one plane down: an index without `WHERE state = 'submitted'` would say "one
/// policy proposal per tenant **ever**" and pass every assertion above while making a
/// tenant whose first proposal was decided unable to author a second version for the
/// rest of time. Both terminal directions are driven — the approved one and the
/// withdrawn one — because the predicate is about leaving `submitted` and not about
/// which door was taken.
#[tokio::test]
async fn a_decided_policy_unit_holds_nothing_and_the_tenant_proposes_again() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);

    for (n, (id, next, decision, approver)) in [
        (
            Uuid::from_u128(0xc1),
            Uuid::from_u128(0xc2),
            ApprovalDecision::Approve,
            Some(APPROVER),
        ),
        (
            Uuid::from_u128(0xc3),
            Uuid::from_u128(0xc4),
            ApprovalDecision::Void,
            None,
        ),
    ]
    .iter()
    .enumerate()
    {
        let version = u64::try_from(n).expect("two rounds") * 2;
        approval_repo::open(
            &conn,
            &scope,
            policy_proposal(*id, TENANT, version),
            stamp_of(SUBMITTER, at(9)),
        )
        .await
        .expect("the proposal opens");
        approval_repo::decide(
            &conn,
            &scope,
            TENANT,
            *id,
            *decision,
            *approver,
            // Neither terminal direction here carries one: `inst-as-reject` is the
            // only decision that requires a reason, and a reject would be a third
            // round saying nothing this pair does not.
            None,
            stamp_of(APPROVER, at(11)),
        )
        .await
        .expect("the decision lands");

        approval_repo::open(
            &conn,
            &scope,
            policy_proposal(*next, TENANT, version + 1),
            stamp_of(SUBMITTER, at(12)),
        )
        .await
        .unwrap_or_else(|e| {
            panic!("a tenant whose proposal left `submitted` must be able to author another: {e}")
        });
        approval_repo::decide(
            &conn,
            &scope,
            TENANT,
            *next,
            ApprovalDecision::Void,
            None,
            None,
            stamp_of(APPROVER, at(13)),
        )
        .await
        .expect("clear the way for the next round");
    }
}

/// One tenant's open proposal is not another tenant's.
///
/// The conjunct an index on the wrong columns loses first. `(tenant_id)` under the
/// predicate is the rule; a unique index on `subject_kind` alone — or on nothing but
/// the predicate — would make the whole deployment hold one policy proposal at a
/// time, which no test above can see because they all use one tenant.
#[tokio::test]
async fn the_guard_is_per_tenant_and_not_per_deployment() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");

    for (id, tenant) in [
        (Uuid::from_u128(0xe1), TENANT),
        (Uuid::from_u128(0xe2), OTHER_TENANT),
    ] {
        approval_repo::open(
            &conn,
            &AccessScope::for_tenant(tenant),
            policy_proposal(id, tenant, 0),
            opened_under(),
        )
        .await
        .unwrap_or_else(|e| panic!("tenant {tenant} must be able to open its own proposal: {e}"));
    }
}

/// A **taken `approval_id`** is still `ConcurrentMutation`, and the discriminator is
/// what keeps it so.
///
/// The other half of `policy_guard_or_contention`. Both unique indexes on this table
/// can be violated by a policy insert and the two refusals send an operator to
/// different places — *retry* against *decide the proposal you already have* — so a
/// branch that answered the mint guard for every unique violation would be as wrong
/// as the one that answered contention for all of them. The first unit is **decided**
/// here, so the guard's predicate does not hold and the primary key is the only thing
/// left to refuse the insert; `rest_threshold_policy::a_policy_unit_whose_id_is_taken_is_a_retriable_conflict_and_not_a_storage_failure`
/// is the service-level statement of the same fact.
#[tokio::test]
async fn a_policy_unit_reusing_a_decided_units_id_is_contention_and_not_the_mint_guard() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let taken = Uuid::from_u128(0xd1);

    approval_repo::open(
        &conn,
        &scope,
        policy_proposal(taken, TENANT, 0),
        opened_under(),
    )
    .await
    .expect("the first proposal opens");
    approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        taken,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("approve it, so the mint guard's predicate no longer holds");

    let refused = approval_repo::open(
        &conn,
        &scope,
        policy_proposal(taken, TENANT, 1),
        stamp_of(SUBMITTER, at(12)),
    )
    .await
    .expect_err("the primary key refuses a second unit under one id");

    assert_eq!(
        refused,
        RepoError::ConcurrentMutation {
            aggregate: format!("approval {taken}")
        },
        "a duplicate id is retriable contention; only the mint guard is a pending-unit conflict"
    );
}

// ---------------------------------------------------------------------------
// The material membership-move path (Task 7 of the customer-group plane):
// `inst-mm-immediate`, `inst-mm-bulk`, `inst-mm-pending`. `subject_aggregate`
// and `re_derive` both refused `AuditSubjectKind::Membership` outright before
// this task; these are the tests that pin the opener down.
// ---------------------------------------------------------------------------

/// Declare one **active** customer-group value directly through the entity —
/// `rest_support::declare_customer_group`'s shape, reproduced here because
/// this file drives the repository layer with no `Harness` to hang it off.
async fn declare_customer_group(
    conn: &impl toolkit_db::secure::DBRunner,
    tenant: Uuid,
    value: &str,
) {
    use bss_pricing::infra::storage::entity::customer_group_taxonomy;
    use sea_orm::EntityTrait;

    let row = customer_group_taxonomy::ActiveModel {
        tenant_id: sea_orm::ActiveValue::Set(tenant),
        value: sea_orm::ActiveValue::Set(value.to_owned()),
        display_name: sea_orm::ActiveValue::Set(value.to_owned()),
        state: sea_orm::ActiveValue::Set("active".to_owned()),
    };
    customer_group_taxonomy::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&AccessScope::allow_all(), &row)
        .expect("scope")
        .exec(conn)
        .await
        .expect("declare the customer-group value");
}

/// A registry that hands out one pending handle per `request_id` —
/// `sqlite_publish_commit.rs`'s `RegistryDouble`, reproduced for the same
/// reason `declare_customer_group` above is: this file has no shared harness
/// with that one.
#[derive(Default)]
struct RegistryDouble {
    /// Every `request_id` this double was asked for, in order.
    ///
    /// **Recorded, because the handle is the interaction and the rows are not.**
    /// The replay arm of `commit_membership_move_in` re-requests a version under
    /// the *existing* membership's id; an arm that minted a fresh id, or used the
    /// wrong tenant, would produce the same receipt count, the same
    /// `enrolled.membership_id` and the same green. The double's own doc promises
    /// one handle per `request_id`, and nothing could read that promise back.
    requested: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl CatalogVersionRegistryV1 for RegistryDouble {
    async fn request_version(
        &self,
        _ctx: &SecurityContext,
        request_id: &str,
    ) -> Result<PendingVersionRef, CanonicalError> {
        self.requested
            .lock()
            .expect("the recorder is not poisoned")
            .push(request_id.to_owned());
        Ok(PendingVersionRef {
            request_id: request_id.to_owned(),
            pending_ref: format!("pend-{request_id}"),
        })
    }

    async fn committed_version(
        &self,
        _ctx: &SecurityContext,
        _pending_ref: &str,
    ) -> Result<Option<CatalogVersion>, CanonicalError> {
        Ok(None)
    }
}

fn security_context(principal: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(principal)
        .subject_tenant_id(tenant)
        .build()
        .expect("a subject and a tenant are all a context needs")
}

/// The materiality verdict rendered exactly as a surface would store it —
/// **evaluated, not asserted**: this helper calls `materiality::evaluate`
/// itself rather than taking a literal, so a test using it cannot
/// accidentally prove F-1's mistake (comparing a constant to itself).
fn evaluated_materiality(act: Trigger) -> serde_json::Value {
    let verdict = materiality::evaluate(&ChangeSet::of_act(act, Vec::new()), None, None);
    json!({
        "material": verdict.is_material(),
        "reason": verdict.reason().map(bss_pricing::domain::materiality::MaterialityReason::as_str),
    })
}

/// **The claim this test exists to prove:** a bulk group move — many payers,
/// one unit — is registered as an always-material trigger, and what proves it
/// is the record the *store* holds after a fresh read, never the value handed
/// to `submit_membership_move_on`. Comparing the handler's own literal against
/// itself is review finding F-1 in this crate; this test reads the row back.
///
/// To redden this: comment out the `triggers::triggered(change)` early return
/// in `domain::materiality::evaluate`. This change set then falls through to
/// `inst-mat-failsafe` and stores `noConfiguredThreshold` instead of
/// `alwaysMaterialTrigger`. Proved by doing exactly that (see the report).
#[tokio::test]
async fn a_bulk_group_move_is_always_material_and_the_store_reflects_it() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let approval_id = Uuid::from_u128(0xb0_01);

    let set = MembershipMoveSet::new(vec![
        MembershipMoveProposal {
            payer_tenant_id: Uuid::from_u128(0xb0_11),
            group_value: "gold".to_owned(),
            effective_from: at(9),
        },
        MembershipMoveProposal {
            payer_tenant_id: Uuid::from_u128(0xb0_12),
            group_value: "gold".to_owned(),
            effective_from: at(9),
        },
    ])
    .expect("two distinct payers make a valid set");

    ApprovalService::submit_membership_move_on(
        &conn,
        &scope,
        TENANT,
        &set,
        approval_id,
        evaluated_materiality(Trigger::BulkGroupMove),
        stamp_of(SUBMITTER, at(9)),
    )
    .await
    .expect("open the bulk-move unit");

    let stored = approval_repo::read(&conn, &scope, TENANT, approval_id)
        .await
        .expect("read")
        .expect("the unit exists");
    assert_eq!(stored.subject_kind, AuditSubjectKind::Membership);
    assert_eq!(
        stored.materiality["reason"], "alwaysMaterialTrigger",
        "a bulk group move must be registered as an always-material trigger: {:?}",
        stored.materiality
    );
    assert_eq!(stored.materiality["material"], true);

    // A many-payer unit's own trail lands on `BulkOperation`, not on either
    // payer's chain — `subject_aggregate`'s Membership arm, exercised end to
    // end rather than unit-tested against a bare record.
    let aggregate = approval_repo::subject_aggregate(&stored).expect("resolve the aggregate");
    assert!(
        matches!(aggregate, SubjectAggregate::BulkOperation(_)),
        "a set spanning several payers must not land on one payer's own chain: {aggregate:?}"
    );
}

/// The positive control the sibling test needs: an ordinary change set with
/// **no** registered act reaches a different reason than `alwaysMaterialTrigger`.
/// Without this, `a_bulk_group_move_is_always_material_and_the_store_reflects_it`
/// could be passing because `evaluate` answers `alwaysMaterialTrigger` for
/// everything, which would make the trigger claim vacuous.
#[tokio::test]
async fn an_ordinary_change_set_with_no_registered_act_is_not_forced_material_by_accident() {
    let verdict = materiality::evaluate(&ChangeSet::of_records(Vec::new()), None, None);
    assert_eq!(
        verdict.reason(),
        Some(bss_pricing::domain::materiality::MaterialityReason::NoConfiguredThreshold),
        "a change set carrying no registered act must not answer alwaysMaterialTrigger, or the \
         bulk test's own assertion would be indistinguishable from every other act"
    );
}

/// A **single-payer** material move — `inst-mm-immediate` — lands its own
/// trail on that payer's chain (`SubjectAggregate::Payer`), not on a
/// synthetic bulk one. The sibling proof to the bulk test's `BulkOperation`
/// assertion: one payer and many payers must resolve to genuinely different
/// aggregates, or a reviewer walking one payer's history would see every
/// bulk move that happened to include them too.
#[tokio::test]
async fn a_single_payer_immediate_move_lands_on_that_payers_own_chain() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let payer = Uuid::from_u128(0xb0_21);
    let approval_id = Uuid::from_u128(0xb0_02);

    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: payer,
        group_value: "gold".to_owned(),
        effective_from: at(9),
    }])
    .expect("one payer is a valid set");

    ApprovalService::submit_membership_move_on(
        &conn,
        &scope,
        TENANT,
        &set,
        approval_id,
        evaluated_materiality(Trigger::ImmediateMembershipReresolution),
        stamp_of(SUBMITTER, at(9)),
    )
    .await
    .expect("open the immediate-move unit");

    let stored = approval_repo::read(&conn, &scope, TENANT, approval_id)
        .await
        .expect("read")
        .expect("the unit exists");
    let aggregate = approval_repo::subject_aggregate(&stored).expect("resolve the aggregate");
    assert_eq!(
        aggregate,
        SubjectAggregate::Payer(payer),
        "one payer's own move must land on that payer's own chain"
    );
}

/// The pin re-derives with **no store read at all** — `inst-mm-pending`'s own
/// shape, proved through `ApprovalService::find` rather than asserted from
/// the module doc: the payload lives in `subject_ref` itself, so
/// `content_matches_pin` must read `true` the moment the unit is opened, with
/// nothing else in this crate having been written for it to re-derive from.
///
/// To redden this: replace `AuditSubjectKind::Membership`'s arm in
/// `infra::approval::re_derive` with the pre-Task-7 refusal
/// (`Err(DomainError::Internal(...))`). `ApprovalService::find` then answers
/// `Err` instead of a detail with `content_matches_pin: true`, and the
/// `.expect("find")` below panics.
#[tokio::test]
async fn a_membership_moves_pin_re_derives_with_no_store_to_read_from() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let approval_id = Uuid::from_u128(0xb0_03);

    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: Uuid::from_u128(0xb0_31),
        group_value: "gold".to_owned(),
        effective_from: at(9),
    }])
    .expect("one payer is a valid set");

    ApprovalService::submit_membership_move_on(
        &conn,
        &scope,
        TENANT,
        &set,
        approval_id,
        evaluated_materiality(Trigger::ImmediateMembershipReresolution),
        stamp_of(SUBMITTER, at(9)),
    )
    .await
    .expect("open the unit");

    let service = ApprovalService::new(provider.clone());
    let detail = service
        .find(&scope, TENANT, approval_id, at(10))
        .await
        .expect("find")
        .expect("the unit exists");
    assert!(
        detail.content_matches_pin,
        "a membership move's pin must always match: nothing in this crate can move its own \
         subject_ref between submit and this read"
    );
    match detail.subject {
        Some(PinnedSubject::Membership(re_derived)) => {
            assert_eq!(
                re_derived, set,
                "re-derivation must decode exactly the proposed set"
            );
        }
        other => panic!("expected PinnedSubject::Membership, got {other:?}"),
    }
}

/// **The approve -> commit path works end to end, at three calls and not
/// two.** A material membership move, once approved by an independent second
/// principal, actually applies: no `pricing_group_membership` row exists
/// before the approve (`inst-mm-pending`'s "nothing is written before
/// approval"), and the committed row exists after — an opener that can open
/// but never commit is half a lane. A **second commit call** over the same
/// approved (and therefore terminal) record must not try to re-apply the
/// move: that is the replay guard this test's third act exercises, at the
/// layer `ApprovalService::commit_membership_move_in` owns rather than at
/// the one route that happens to call it today.
///
/// To redden this (the commit half): empty the `for proposal in
/// set.proposals()` loop's body in `commit_membership_move_in`, leaving it a
/// no-op that still returns `Ok(Vec::new())`. The `receipts.len()`
/// assertion and the `intervals_for_payer` read after the first commit both
/// fail.
///
/// To redden this (the replay guard): remove the `already_applied` check
/// from `commit_membership_move_in`. The second commit call then re-enters
/// `move_payer_in` for the proposal the first call already applied, which
/// tries to end the membership row it just created at the exact instant it
/// starts, and `end_membership` refuses `MembershipIntervalEmpty` — the
/// second call's `.expect("commit the approved move a second time")` panics
/// instead of the assertions after it running.
#[tokio::test]
async fn an_approved_material_move_actually_commits() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    declare_customer_group(&conn, TENANT, "gold").await;

    let payer = Uuid::from_u128(0xb0_41);
    let approval_id = Uuid::from_u128(0xb0_04);
    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: payer,
        group_value: "gold".to_owned(),
        effective_from: at(9),
    }])
    .expect("one payer is a valid set");

    ApprovalService::submit_membership_move_on(
        &conn,
        &scope,
        TENANT,
        &set,
        approval_id,
        evaluated_materiality(Trigger::ImmediateMembershipReresolution),
        stamp_of(SUBMITTER, at(9)),
    )
    .await
    .expect("open the unit");

    // Before approval: `inst-mm-pending`'s own promise — nothing written yet.
    let before = group_membership_repo::intervals_for_payer(&conn, &scope, TENANT, payer)
        .await
        .expect("read the payer's intervals");
    assert!(
        before.is_empty(),
        "no membership row exists before the unit is approved"
    );

    let decided = approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        approval_id,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(10)),
    )
    .await
    .expect("approve as the independent second principal");

    let registry = RegistryDouble::default();
    let ctx = security_context(APPROVER, TENANT);
    let receipts = ApprovalService::commit_membership_move_in(
        &conn,
        &registry,
        &ctx,
        &scope,
        TENANT,
        &decided,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("commit the approved move");

    assert_eq!(receipts.len(), 1, "one proposal must produce one receipt");
    assert_eq!(receipts[0].enrolled.group_value, "gold");
    assert_eq!(receipts[0].enrolled.payer_tenant_id, payer);

    let after = group_membership_repo::intervals_for_payer(&conn, &scope, TENANT, payer)
        .await
        .expect("read the payer's intervals");
    assert_eq!(
        after.len(),
        1,
        "the approved move must actually write the membership row, not merely answer a receipt"
    );

    // **The third act.** `decided` is still `Approved` — nothing in this
    // crate moves a decided record — so a second commit call over it must
    // answer idempotently rather than trying to re-apply the move.
    let replayed_receipts = ApprovalService::commit_membership_move_in(
        &conn,
        &registry,
        &ctx,
        &scope,
        TENANT,
        &decided,
        stamp_of(APPROVER, at(12)),
    )
    .await
    .expect("commit the approved move a second time");
    assert_eq!(
        replayed_receipts.len(),
        1,
        "a replay still answers one receipt"
    );
    assert_eq!(
        replayed_receipts[0].enrolled.membership_id, receipts[0].enrolled.membership_id,
        "a replay must report the same membership the first commit created, not a new one"
    );
    // The handle, which is what the replay arm is *for*. Same membership id in the
    // receipt is not the same handle: an arm re-requesting under a freshly minted
    // id would still answer the stored membership.
    assert_eq!(
        replayed_receipts[0].pending_ref, receipts[0].pending_ref,
        "a replay must land on the handle the first commit already has"
    );
    // And the id it was requested under is derived from the membership, not from
    // the proposal — `infra::membership_publish::move_request_id`'s format, spelled
    // here because it is `pub(crate)` and this is an integration test.
    let asked = registry
        .requested
        .lock()
        .expect("the recorder is not poisoned")
        .clone();
    let expected = format!(
        "membership-move/{TENANT}/{}",
        receipts[0].enrolled.membership_id
    );
    assert_eq!(
        asked,
        vec![expected.clone(), expected],
        "both commits must ask under the membership's own id, once each"
    );

    let after_replay = group_membership_repo::intervals_for_payer(&conn, &scope, TENANT, payer)
        .await
        .expect("read the payer's intervals");
    assert_eq!(
        after_replay.len(),
        1,
        "a replayed commit must not write a second membership row"
    );
}

/// The two-person rule still holds at commit: a record naming one principal
/// as both submitter and approver authorizes nothing, even though its
/// content cannot have moved the way a plan's can.
///
/// To redden this: remove the `independent_approver(approved)?` call from
/// `ApprovalService::commit_membership_move_in`. This test then commits a
/// hand-crafted self-approved record successfully instead of refusing it.
#[tokio::test]
async fn a_self_approved_membership_record_still_commits_nothing() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    declare_customer_group(&conn, TENANT, "gold").await;

    let payer = Uuid::from_u128(0xb0_51);
    let set = MembershipMoveSet::new(vec![MembershipMoveProposal {
        payer_tenant_id: payer,
        group_value: "gold".to_owned(),
        effective_from: at(9),
    }])
    .expect("one payer is a valid set");

    // A record this store's own CHECKs would refuse if it went through
    // `decide` — built by hand precisely to prove the commit path re-checks
    // rather than trusting a stored `approver_principal` column, exactly as
    // `independent_approver`'s own doc argues for a row "written around the
    // store".
    let approval_id = Uuid::from_u128(0xb0_05);
    let forged = ApprovalRecord {
        approval_id,
        tenant_id: TENANT,
        subject_ref: approval_repo::membership_move_subject_ref(&set).expect("encode"),
        subject_kind: AuditSubjectKind::Membership,
        content_hash: bss_pricing::domain::approval::content_pin::membership_content_hash(&set)
            .to_vec(),
        state: ApprovalState::Approved,
        submitter_principal: SUBMITTER,
        approver_principal: Some(SUBMITTER),
        reason: None,
        materiality: evaluated_materiality(Trigger::ImmediateMembershipReresolution),
        submitted_at: at(9),
        decided_at: Some(at(10)),
    };

    let registry = RegistryDouble::default();
    let ctx = security_context(SUBMITTER, TENANT);
    let err = ApprovalService::commit_membership_move_in(
        &conn,
        &registry,
        &ctx,
        &scope,
        TENANT,
        &forged,
        stamp_of(SUBMITTER, at(11)),
    )
    .await
    .expect_err("a self-approved record must authorize nothing");
    assert!(
        matches!(
            err,
            bss_pricing::domain::error::DomainError::SelfApprovalForbidden(_)
        ),
        "expected SelfApprovalForbidden, got {err:?}"
    );

    let after = group_membership_repo::intervals_for_payer(&conn, &scope, TENANT, payer)
        .await
        .expect("read the payer's intervals");
    assert!(after.is_empty(), "a refused commit must write nothing");
}

/// Direct aggregate kinds are included; decided, foreign, indirect and
/// off-page units are excluded. Multiple units survive with stable ordering.
#[tokio::test]
async fn pending_plan_units_are_scoped_ordered_and_include_all_direct_kinds() {
    let provider = harness().await;
    let conn = provider.conn().expect("scoped connection");
    let scope = AccessScope::for_tenant(TENANT);
    let one = Uuid::from_u128(0x9_a1);
    let two = Uuid::from_u128(0x9_a2);
    let decided_plan = Uuid::from_u128(0x9_a3);
    let windowed_plan = Uuid::from_u128(0x9_a4);
    let foreign_plan = Uuid::from_u128(0x9_a5);

    for (unit, plan) in [(0xb1_u128, one), (0xb2, two)] {
        approval_repo::open(
            &conn,
            &scope,
            pending_over(
                Uuid::from_u128(unit),
                TENANT,
                &format!("{plan}/1"),
                AuditSubjectKind::PlanRevision,
            ),
            opened_under(),
        )
        .await
        .expect("open a pending plan unit");
    }

    // Decided: out of the set, and the only one of the four whose exclusion the
    // `state` predicate is responsible for.
    let approved = Uuid::from_u128(0xb3);
    approval_repo::open(
        &conn,
        &scope,
        pending_over(
            approved,
            TENANT,
            &format!("{decided_plan}/1"),
            AuditSubjectKind::PlanRevision,
        ),
        opened_under(),
    )
    .await
    .expect("open");
    approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        approved,
        ApprovalDecision::Approve,
        Some(APPROVER),
        None,
        stamp_of(APPROVER, at(11)),
    )
    .await
    .expect("approve it");

    // A window belongs directly to its plan, despite not reviewing a revision.
    approval_repo::open(
        &conn,
        &scope,
        pending_over(
            Uuid::from_u128(0xb4),
            TENANT,
            &format!("{windowed_plan}/1"),
            AuditSubjectKind::Window,
        ),
        opened_under(),
    )
    .await
    .expect("open");

    // Another tenant's pending plan unit.
    approval_repo::open(
        &conn,
        &AccessScope::for_tenant(OTHER_TENANT),
        pending_over(
            Uuid::from_u128(0xb5),
            OTHER_TENANT,
            &format!("{foreign_plan}/1"),
            AuditSubjectKind::PlanRevision,
        ),
        opened_under(),
    )
    .await
    .expect("open");

    // Insert in reverse id order, with equal timestamps, to exercise the tie-break.
    for (id, kind, subject) in [
        (
            0xb7,
            AuditSubjectKind::PriceUnit,
            format!("{one}/price-change"),
        ),
        (
            0xb6,
            AuditSubjectKind::Window,
            format!("{one}/window-change"),
        ),
        (0xb8, AuditSubjectKind::Policy, "tenant-policy".to_owned()),
    ] {
        approval_repo::open(
            &conn,
            &scope,
            pending_over(Uuid::from_u128(id), TENANT, &subject, kind),
            opened_under(),
        )
        .await
        .expect("open additional unit");
    }
    let requested: Vec<PlanId> = [one, two, decided_plan, windowed_plan, foreign_plan]
        .into_iter()
        .map(PlanId::new)
        .collect();
    let pending = approval_repo::pending_for_plans(&conn, &scope, TENANT, &requested)
        .await
        .expect("read the pending set");
    let held: std::collections::BTreeSet<Uuid> = pending.keys().map(|plan| plan.get()).collect();
    assert_eq!(
        held,
        [one, two, windowed_plan]
            .into_iter()
            .collect::<std::collections::BTreeSet<Uuid>>(),
        "only requested plans with direct submitted units"
    );
    assert_eq!(
        pending[&PlanId::new(one)]
            .iter()
            .map(|unit| unit.approval_id)
            .collect::<Vec<_>>(),
        [0xb1, 0xb6, 0xb7].map(Uuid::from_u128)
    );
    let page = approval_repo::pending_for_plans(&conn, &scope, TENANT, &[PlanId::new(two)])
        .await
        .expect("one-plan page");
    assert_eq!(page.len(), 1);
    assert!(page.contains_key(&PlanId::new(two)));
    assert!(
        approval_repo::pending_for_plans(&conn, &scope, TENANT, &[])
            .await
            .expect("empty page")
            .is_empty()
    );
    assert_eq!(
        approval_repo::find_pending_for_plan(&conn, &scope, TENANT, PlanId::new(one))
            .await
            .expect("revision write guard")
            .expect("revision")
            .subject_kind,
        AuditSubjectKind::PlanRevision
    );
    assert!(
        approval_repo::find_pending_for_plan(&conn, &scope, TENANT, PlanId::new(windowed_plan))
            .await
            .expect("window is not a revision conflict")
            .is_none()
    );
}

/// Batch taxonomy joins distinguish tenant, class and value, with an explicit
/// stable order. Approval-read visibility retains its resource constraints.
#[tokio::test]
async fn pending_taxonomy_units_and_preview_visibility_are_scoped_independently() {
    use bss_pricing::domain::overlay::ScopeValue;
    use bss_pricing::domain::taxonomy::{TaxonomyClass, TaxonomyValuePatch, TaxonomyValueProposal};
    use toolkit_security::{ScopeConstraint, ScopeFilter, pep_properties};

    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let scope = AccessScope::for_tenant(TENANT);
    for (id, tenant, class, value) in [
        (3, TENANT, TaxonomyClass::Brand, "same"),
        (2, TENANT, TaxonomyClass::Brand, "same"),
        (1, TENANT, TaxonomyClass::Brand, "same"),
        (4, TENANT, TaxonomyClass::Region, "same"),
        (5, TENANT, TaxonomyClass::Brand, "off-page"),
        (6, OTHER_TENANT, TaxonomyClass::Brand, "same"),
    ] {
        let proposal = TaxonomyValueProposal {
            class,
            value: ScopeValue::new(value).unwrap(),
            patch: TaxonomyValuePatch {
                display_name: Some(format!("Label {id}")),
                ..TaxonomyValuePatch::default()
            },
        };
        approval_repo::open(
            &conn,
            &AccessScope::for_tenant(tenant),
            pending_over(
                Uuid::from_u128(id),
                tenant,
                &approval_repo::taxonomy_value_subject_ref(&proposal).expect("encode"),
                AuditSubjectKind::TaxonomyValue,
            ),
            opened_under(),
        )
        .await
        .expect("open");
    }
    approval_repo::decide(
        &conn,
        &scope,
        TENANT,
        Uuid::from_u128(3),
        ApprovalDecision::Reject,
        Some(APPROVER),
        Some("not needed".to_owned()),
        stamp_of(APPROVER, at(10)),
    )
    .await
    .expect("decide");
    let units = approval_repo::pending_for_taxonomy_values(
        &conn,
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        &["same".to_owned()],
    )
    .await
    .expect("batch read");
    assert_eq!(units.len(), 1);
    assert_eq!(
        units["same"]
            .iter()
            .map(|unit| unit.record.approval_id)
            .collect::<Vec<_>>(),
        [1, 2].map(Uuid::from_u128)
    );
    assert_eq!(
        units["same"][0].proposal.patch.display_name.as_deref(),
        Some("Label 1")
    );

    let pinned = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![TENANT]),
        ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![Uuid::from_u128(2)]),
    ]));
    let visible =
        approval_repo::visible_ids(&conn, &pinned, TENANT, &[1, 2, 6].map(Uuid::from_u128))
            .await
            .expect("readable intersection");
    assert_eq!(visible, [Uuid::from_u128(2)].into_iter().collect());
    assert!(
        approval_repo::visible_ids(
            &conn,
            &AccessScope::for_tenant(OTHER_TENANT),
            TENANT,
            &[1, 2, 6].map(Uuid::from_u128)
        )
        .await
        .expect("foreign scope")
        .is_empty()
    );
    assert!(
        approval_repo::pending_for_taxonomy_values(
            &conn,
            &AccessScope::for_tenant(OTHER_TENANT),
            TENANT,
            TaxonomyClass::Brand,
            &["same".to_owned()]
        )
        .await
        .expect("foreign scope")
        .is_empty()
    );
    assert!(
        approval_repo::pending_for_taxonomy_values(
            &conn,
            &scope,
            TENANT,
            TaxonomyClass::Brand,
            &[]
        )
        .await
        .expect("empty values")
        .is_empty()
    );
    assert!(
        approval_repo::visible_ids(&conn, &pinned, TENANT, &[])
            .await
            .expect("empty ids")
            .is_empty()
    );
}

/// Corruption is a failure, not "nothing pending", and diagnostics cannot leak
/// the proposal through a metadata-only config read.
#[tokio::test]
async fn a_malformed_pending_taxonomy_subject_fails_without_exposing_content() {
    use bss_pricing::domain::taxonomy::TaxonomyClass;
    use bss_pricing::infra::storage::entity::approval;
    use sea_orm::{ActiveValue::Set, EntityTrait};

    let provider = harness().await;
    let conn = provider.conn().expect("conn");
    let scope = AccessScope::for_tenant(TENANT);
    let row = approval::ActiveModel {
        approval_id: Set(Uuid::from_u128(0xc077)),
        tenant_id: Set(TENANT),
        subject_ref: Set("taxonomy-value/secret-proposal-invalid-json".to_owned()),
        subject_kind: Set("taxonomy_value".to_owned()),
        content_hash: Set(vec![1; 32]),
        state: Set("submitted".to_owned()),
        submitter_principal: Set(SUBMITTER),
        approver_principal: Set(None),
        reason: Set(None),
        materiality: Set(json!({})),
        submitted_at: Set(at(9)),
        decided_at: Set(None),
    };
    approval::Entity::insert(row.clone())
        .secure()
        .scope_with_model(&scope, &row)
        .expect("scope")
        .exec(&conn)
        .await
        .expect("insert corrupt fixture");
    let err = approval_repo::pending_for_taxonomy_values(
        &conn,
        &scope,
        TENANT,
        TaxonomyClass::Brand,
        &["acme".to_owned()],
    )
    .await
    .expect_err("corrupt proposal");
    assert!(matches!(err, RepoError::CorruptRow(_)));
    assert!(!err.to_string().contains("secret-proposal"));
}
