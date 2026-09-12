//! The TOCTOU guard's race, on real Postgres: a mutation of the pinned subject
//! and an approve of its record, concurrently.
//!
//! # Why this cannot be a `SQLite` suite
//!
//! `sqlite::memory:` **serializes writers**, so the interleaving under test can
//! be neither confirmed nor refuted there — not "probably fine", unanswered in
//! both directions. `tests/sqlite_approval_service.rs` proves that a mutation
//! voids a pending unit and that a decided unit refuses a second decision; what
//! it cannot show is what happens when the two statements are **in flight at the
//! same time**, which is the whole hazard `inst-ap-pin` names.
//!
//! # The choreography, and why each step is there
//!
//! A concurrency test that starts two tasks and asserts on the outcome is a coin
//! toss with a green side. Both races here are driven by observable events only,
//! in the idiom `tests/postgres_audit_chain.rs` established:
//!
//! 1. the mutating transaction runs its write — which includes the void — and
//!    then **parks**, holding the row lock on the pending record;
//! 2. the approving call starts; its read sees the still-committed `submitted`
//!    row, and its compare-and-swap blocks on that lock;
//! 3. a third connection **observes the block** in `pg_locks`, which is what
//!    proves the approve's read already happened;
//! 4. only then is the mutation released to commit, and the approve's `UPDATE`
//!    re-evaluates its `state = 'submitted'` predicate under READ COMMITTED,
//!    matches nothing, and resolves into the refusal.
//!
//! Step 3 is the load-bearing one. Without it the approve could read the *voided*
//! row and be refused by its own pendingness check before ever contending —
//! green, and about nothing.
//!
//! # What "exactly one wins" means here, precisely
//!
//! The contended object is the **pending record**, and what is at stake is which
//! of two writes to it takes effect. The two orderings are not symmetric and
//! both are asserted:
//!
//! - **The void commits first.** The approve loses and is answered
//!   `APPROVAL_NOT_PENDING` — a **409**, the outcome §5 specifies — rather than
//!   the driver error a repository that trusted its read would produce, which
//!   reaches an operator as a 500 about a race whose whole remedy is to re-read.
//! - **The approve commits first.** The void's `UPDATE` matches nothing and the
//!   mutation still commits. That is deliberate and load-bearing rather than a
//!   loss: `inst-as-immutable` makes a decided record untouchable, so a void that
//!   tried to close it would be refused by
//!   `trg_pricing_approval_append_only` — and because the void runs inside the
//!   mutation's transaction, that refusal would roll the **mutation** back. Every
//!   authoring edit to a plan that had ever been approved would fail.
//!
//! The residue of the second case is reported rather than hidden: an approval
//! that has already been granted is **not** re-invalidated by a later mutation.
//! `inst-ap-pin` scopes the void to a `submitted` record in as many words ("any
//! mutation of the subject **while `submitted`**"), so this is the design set's
//! own boundary and not a hole in the implementation of it — but it means the
//! window between `approved` and the publish commit is guarded by nothing here.
//! Closing it belongs to the publish handler, which holds the record's
//! `content_hash` and can re-verify the pin before handing
//! `PublishAuthorization::Approved` to `commit`.
//!
//! # The mint guard's two cases, and what each of them would fail against
//!
//! The last two cases in this file are D-192's, and they are about different halves of
//! one guard. [`the_policy_mint_guard_answers_a_pending_unit_conflict_on_postgres`] pins
//! its **classification** — that a `unique_violation` over
//! `uq_pricing_approval_policy_pending` is read as the mint guard and not as the table's
//! primary key — and needs no race at all; it would fail against a Postgres that stopped
//! rendering the index name.
//! [`two_proposals_intersecting_on_a_currency_meet_the_mint_guard_and_not_the_version_key`]
//! pins its **ordering**: that the guard stands *above* the mint rather than below it. It
//! would fail against a `ThresholdService::propose` that writes the
//! version rows first — the loser then met
//! `pricing_approval_threshold`'s `(tenant, version, currency)` key instead of the guard,
//! and a violation of *that* key is a bare `RepoError::Db` rendering **500**, a server
//! fault for a race whose whole remedy is to decide or withdraw the proposal the tenant
//! already holds. Swap the two writes back in `propose` and that case reddens on the
//! refusal's shape, which is the only thing the swap moves.
//!
//! Ignored by default; they need Docker. Run with
//! `cargo test -p cf-gears-bss-pricing --test postgres_approval_race -- --ignored`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod pg_support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bss_pricing::domain::approval::{ApprovalState, DecisionBy, WithdrawAuthority};
use bss_pricing::domain::audit::AuditStamp;
use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::instant::utc_ymd_hms;
use bss_pricing::domain::materiality::{ThresholdBasis, ThresholdEntry};
use bss_pricing::domain::money::{CurrencyCode, MinorAmount};
use bss_pricing::domain::plan_shape::{
    BillingCycle, DescriptorSet, Frequency, PhaseKind, PlanPhase,
};
use bss_pricing::domain::price_record::PriceContent;
use bss_pricing::domain::price_row::{ModelKind, PriceRow};
use bss_pricing::domain::scope_key::{
    ChargeKind, Cohort, PhaseId, PlanId, PriceEligibility, Region, ScopeKey,
};
use bss_pricing::infra::approval::{ApprovalService, DecideRequest, RegionGrant};
use bss_pricing::infra::storage::RepoError;
use bss_pricing::infra::storage::repo::price_repo::{self, NewPriceDraft};
use bss_pricing::infra::storage::repo::{
    PlanRepo, PlanShapeRepo, PriceRepo, ThresholdEntryRow, threshold_repo,
};
use bss_pricing::infra::threshold::{AssertedPolicy, ThresholdService};
use time::OffsetDateTime;

use pg_support::Pg;
use serde_json::json;
use tokio::sync::Notify;
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, DbError};
use uuid::Uuid;

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

const TENANT: Uuid = Uuid::from_u128(0x7e_11);
const SUBMITTER: Uuid = Uuid::from_u128(0x5b_01);
const APPROVER: Uuid = Uuid::from_u128(0xab_01);

/// Generous, because a cold container under load is slow — but **finite**: a
/// racer that never resolves is a refuted claim, not a slow one.
const RACE_TIMEOUT: Duration = Duration::from_secs(30);

fn plan_id() -> PlanId {
    PlanId::new(Uuid::from_u128(0x9_1a4))
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

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT)
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

fn new_price(market: &str, seed: u128) -> NewPriceDraft {
    NewPriceDraft {
        price_id: Uuid::from_u128(seed),
        scope_key: scope_key(market),
        content: flat_row(),
        created_by: SUBMITTER,
        created_at_utc: at(10),
        correlation_id: TEST_CORRELATION,
    }
}

/// One plan with a phase chain, a descriptor set and one `eu` row, committed —
/// so both racers below read one world.
async fn seed(pg: &Pg) {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let plans = PlanRepo::new(provider.clone());
    let shapes = PlanShapeRepo::new(provider.clone());
    let prices = PriceRepo::new(provider.clone());
    let scope = scope();

    let created = plans
        .create_draft(
            &scope,
            bss_pricing::infra::storage::repo::plan_repo::NewPlanDraft {
                plan_name: None,
                plan_id: plan_id(),
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
            },
        )
        .await
        .expect("create the draft");
    let after_phases = shapes
        .replace_phases(
            &scope,
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
    shapes
        .set_descriptor_set(
            &scope,
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
    prices
        .create_draft(&scope, TENANT, new_price("eu", 0xb_0001))
        .await
        .expect("author the price row");
}

/// Open the pending unit whose fate the two racers decide.
async fn submit(pg: &Pg, approval_id: Uuid) {
    let approvals = ApprovalService::new(DBProvider::<DbError>::new(pg.db().await));
    approvals
        .submit(
            &scope(),
            TENANT,
            plan_id(),
            approval_id,
            json!({ "reason": "noConfiguredThreshold" }),
            stamp_of(SUBMITTER, at(12)),
        )
        .await
        .expect("open the pending unit");
}

fn approve(approval_id: Uuid) -> DecideRequest {
    DecideRequest {
        approval_id,
        decision: DecisionBy::Approve(APPROVER),
        reason: None,
        approver_regions: RegionGrant::Explicit(BTreeSet::from([
            Region::new("eu").expect("a non-blank region")
        ])),
        stamp: stamp_of(APPROVER, at(13)),
        withdraw_authority: WithdrawAuthority::OwnUnitsOnly,
    }
}

async fn state_of(pg: &Pg, approval_id: Uuid) -> ApprovalState {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");
    bss_pricing::infra::storage::repo::approval_repo::read(&conn, &scope(), TENANT, approval_id)
        .await
        .expect("read the unit")
        .expect("the unit is there")
        .state
}

// ---------------------------------------------------------------------------
// The world in which the race means something
// ---------------------------------------------------------------------------

/// **Uncontended, the approve succeeds.**
///
/// Without this the race below would pass against a service that refuses every
/// approve, and the whole suite would be evidence about nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn an_uncontended_approve_succeeds() {
    let pg = Pg::applied().await;
    seed(&pg).await;
    let id = Uuid::from_u128(0xa1);
    submit(&pg, id).await;

    let approvals = ApprovalService::new(DBProvider::<DbError>::new(pg.db().await));
    let decided = approvals
        .decide(&scope(), TENANT, approve(id))
        .await
        .expect("nothing is contending");
    assert_eq!(decided.state, ApprovalState::Approved);
}

/// The taxonomy approve/apply boundary also works under `PostgreSQL`'s real
/// append-only approval and hash-chain triggers, not only the `SQLite` mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_taxonomy_approve_applies_once_with_its_audit_on_postgres() {
    use bss_pricing::domain::overlay::ScopeValue;
    use bss_pricing::domain::taxonomy::{
        TaxonomyClass, TaxonomyEntry, TaxonomyState, TaxonomyValueChange, TaxonomyValuePatch,
        TaxonomyValueProposal,
    };
    use bss_pricing::infra::storage::repo::approval_repo;
    use bss_pricing::infra::storage::repo::taxonomy_repo::{self, TaxonomyRepo};
    use sea_orm::{ConnectionTrait, Statement};
    use toolkit_security::{ScopeConstraint, ScopeFilter, pep_properties};

    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let held = TaxonomyEntry {
        value: ScopeValue::new("acme").expect("value"),
        display_name: "Acme".to_owned(),
        state: TaxonomyState::Active,
        tax: None,
    };
    TaxonomyRepo::new(provider.clone())
        .declare_value(
            &scope(),
            TENANT,
            TaxonomyClass::Brand,
            held.clone(),
            stamp(),
        )
        .await
        .expect("declare");
    let change = TaxonomyValueChange {
        proposal: TaxonomyValueProposal {
            class: TaxonomyClass::Brand,
            value: held.value.clone(),
            patch: TaxonomyValuePatch {
                display_name: Some("ACME Ltd".to_owned()),
                ..TaxonomyValuePatch::default()
            },
        },
        held,
    };
    let id = Uuid::from_u128(0xa4);
    let conn = provider.conn().expect("conn");
    ApprovalService::submit_taxonomy_value_on(
        &conn,
        &scope(),
        TENANT,
        &change,
        id,
        json!({}),
        stamp(),
    )
    .await
    .expect("submit");
    let pending = approval_repo::pending_for_taxonomy_values(
        &conn,
        &scope(),
        TENANT,
        TaxonomyClass::Brand,
        &["acme".to_owned()],
    )
    .await
    .expect("pending metadata on PostgreSQL");
    assert_eq!(pending["acme"].len(), 1);
    assert_eq!(pending["acme"][0].record.approval_id, id);
    assert_eq!(pending["acme"][0].proposal, change.proposal);
    let readable_scope = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![TENANT]),
        ScopeFilter::in_uuids(pep_properties::RESOURCE_ID, vec![id]),
    ]));
    assert_eq!(
        approval_repo::visible_ids(&conn, &readable_scope, TENANT, &[id])
            .await
            .expect("scoped preview access"),
        BTreeSet::from([id])
    );
    let service = ApprovalService::new(provider.clone());
    let mut decision = approve(id);
    decision.approver_regions = RegionGrant::Explicit(BTreeSet::new());
    service
        .decide(&scope(), TENANT, decision.clone())
        .await
        .expect("approve and apply");
    assert_eq!(state_of(&pg, id).await, ApprovalState::Approved);
    assert!(
        approval_repo::pending_for_taxonomy_values(
            &conn,
            &scope(),
            TENANT,
            TaxonomyClass::Brand,
            &["acme".to_owned()],
        )
        .await
        .expect("decided is no longer pending")
        .is_empty()
    );
    assert_eq!(
        taxonomy_repo::find_value_on(
            &conn,
            &scope(),
            TENANT,
            TaxonomyClass::Brand,
            &change.proposal.value
        )
        .await
        .expect("read")
        .expect("value")
        .display_name,
        "ACME Ltd"
    );
    assert!(matches!(
        service.decide(&scope(), TENANT, decision).await,
        Err(DomainError::ApprovalNotPending(_))
    ));
    let row = pg.raw().await.query_one_raw(Statement::from_string(
        sea_orm::DatabaseBackend::Postgres,
        format!("SELECT COUNT(*) AS n FROM bss.pricing_audit_log WHERE tenant_id = '{TENANT}' \
                 AND subject_ref = 'taxonomy/brand/acme' AND action = 'update' AND approval_ref = '{id}'"),
    )).await.expect("audit query").expect("count");
    assert_eq!(row.try_get::<i64>("", "n").expect("count"), 1);
}

// ---------------------------------------------------------------------------
// The race
// ---------------------------------------------------------------------------

/// Both reviewers read the same before-value. Hold the taxonomy row until both
/// transactions demonstrably wait on database locks, then release it: at most
/// one proposal and its decision/audits may commit. The loser stays submitted
/// and a retry sees a stale pin, not a second successful rename.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_taxonomy_approvals_in_flight_apply_exactly_one_proposal() {
    use bss_pricing::domain::overlay::ScopeValue;
    use bss_pricing::domain::taxonomy::{
        TaxonomyClass, TaxonomyEntry, TaxonomyState, TaxonomyValueChange, TaxonomyValuePatch,
        TaxonomyValueProposal,
    };
    use bss_pricing::infra::storage::repo::taxonomy_repo::{self, TaxonomyRepo};
    use sea_orm::{ConnectionTrait, Statement, TransactionTrait};

    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let held = TaxonomyEntry {
        value: ScopeValue::new("acme").expect("value"),
        display_name: "Acme".to_owned(),
        state: TaxonomyState::Active,
        tax: None,
    };
    TaxonomyRepo::new(provider.clone())
        .declare_value(
            &scope(),
            TENANT,
            TaxonomyClass::Brand,
            held.clone(),
            stamp(),
        )
        .await
        .expect("declare");
    let ids = [Uuid::from_u128(0xa5), Uuid::from_u128(0xa6)];
    let conn = provider.conn().expect("conn");
    for (id, label) in ids.into_iter().zip(["ACME Ltd", "ACME Corp"]) {
        ApprovalService::submit_taxonomy_value_on(
            &conn,
            &scope(),
            TENANT,
            &TaxonomyValueChange {
                held: held.clone(),
                proposal: TaxonomyValueProposal {
                    class: TaxonomyClass::Brand,
                    value: held.value.clone(),
                    patch: TaxonomyValuePatch {
                        display_name: Some(label.to_owned()),
                        ..TaxonomyValuePatch::default()
                    },
                },
            },
            id,
            json!({}),
            stamp(),
        )
        .await
        .expect("submit independent proposal");
    }
    let observer = pg.raw().await;
    let lock = observer.begin().await.expect("blocking transaction");
    lock.query_one_raw(Statement::from_string(sea_orm::DatabaseBackend::Postgres,
        format!("SELECT value FROM bss.pricing_brand_taxonomy WHERE tenant_id = '{TENANT}' AND value = 'acme' FOR UPDATE"),
    )).await.expect("hold taxonomy row").expect("held value");
    let mut racers = Vec::new();
    for id in ids {
        let service = ApprovalService::new(DBProvider::<DbError>::new(pg.db().await));
        racers.push(tokio::spawn(async move {
            let mut request = approve(id);
            request.approver_regions = RegionGrant::Explicit(BTreeSet::new());
            (id, service.decide(&scope(), TENANT, request).await)
        }));
    }
    tokio::time::timeout(RACE_TIMEOUT, async {
        while pg_support::blocked_backends(&observer).await < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both decisions must be in flight before release");
    lock.commit().await.expect("release taxonomy row");

    let mut winner = None;
    let mut loser = None;
    for racer in racers {
        let (id, result) = tokio::time::timeout(RACE_TIMEOUT, racer)
            .await
            .expect("decision completes")
            .expect("no task panic");
        match result {
            Ok(record) => {
                assert_eq!(record.state, ApprovalState::Approved);
                assert!(winner.replace(id).is_none(), "only one winner");
            }
            Err(DomainError::ConcurrentMutation(_) | DomainError::ApprovalContentMismatch(_)) => {
                assert!(loser.replace(id).is_none(), "only one loser");
            }
            other => panic!("expected success or typed concurrency refusal: {other:?}"),
        }
    }
    let winner = winner.expect("one winner");
    let loser = loser.expect("one loser");
    assert_eq!(state_of(&pg, loser).await, ApprovalState::Submitted);
    let value =
        taxonomy_repo::find_value_on(&conn, &scope(), TENANT, TaxonomyClass::Brand, &held.value)
            .await
            .expect("read")
            .expect("value");
    assert_eq!(
        value.display_name,
        if winner == ids[0] {
            "ACME Ltd"
        } else {
            "ACME Corp"
        }
    );
    let mut retry = approve(loser);
    retry.approver_regions = RegionGrant::Explicit(BTreeSet::new());
    assert!(matches!(
        ApprovalService::new(provider)
            .decide(&scope(), TENANT, retry)
            .await,
        Err(DomainError::ApprovalContentMismatch(_))
    ));
    let audit = observer.query_one_raw(Statement::from_string(sea_orm::DatabaseBackend::Postgres,
        format!("SELECT COUNT(*) AS n FROM bss.pricing_audit_log WHERE tenant_id = '{TENANT}' AND approval_ref IN ('{}', '{}') AND action IN ('approve', 'update')", ids[0], ids[1]),
    )).await.expect("audit query").expect("count");
    assert_eq!(
        audit.try_get::<i64>("", "n").expect("count"),
        2,
        "one decision and one apply, no loser residue"
    );
}

/// The mutation commits first: the approve is `APPROVAL_NOT_PENDING`, **not** a
/// storage fault.
///
/// The two are genuinely in flight — the approve's compare-and-swap is observed
/// blocked on the row lock the mutation holds before the mutation is released —
/// so this is the interleaving `inst-ap-pin` is about and not a sequence dressed
/// up as one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_mutation_and_an_approve_in_flight_leave_the_unit_voided_and_the_approve_refused() {
    let pg = Pg::applied().await;
    seed(&pg).await;
    let id = Uuid::from_u128(0xa2);
    submit(&pg, id).await;

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    // The mutation. `create_draft_on` is the runner-taking form of the price
    // create, which is what lets the transaction be held open here — the
    // method opens its own and could not be parked mid-flight. It is the same
    // code path: `record_price_mutation`, and therefore the same void.
    let mutation = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        Box::pin(price_repo::create_draft_on(
                            txn,
                            &scope(),
                            TENANT,
                            new_price("us", 0xb_0002),
                        ))
                        .await?;
                        // The void has run and is uncommitted; the pending
                        // record's row lock is held.
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let approval = {
        let db = pg.db().await;
        tokio::spawn(async move {
            ApprovalService::new(DBProvider::<DbError>::new(db))
                .decide(&scope(), TENANT, approve(id))
                .await
        })
    };

    // The approve's read has happened and its UPDATE is waiting on the lock. Only
    // now is the mutation allowed to commit.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, mutation)
        .await
        .expect("the mutation must finish once released")
        .expect("its task must not panic")
        .expect("the mutation itself is uncontended and must commit");

    let refusal = tokio::time::timeout(RACE_TIMEOUT, approval)
        .await
        .expect("the approve must be released by the mutation's commit")
        .expect("its task must not panic")
        .expect_err("the unit was voided under it");

    assert!(
        matches!(refusal, DomainError::ApprovalNotPending(_)),
        "the loser must be told to re-read, not that the store failed: {refusal:?}"
    );
    assert_eq!(state_of(&pg, id).await, ApprovalState::Voided);
}

/// The approve commits first: the later mutation still commits, and the decided
/// unit is untouched.
///
/// The other ordering, and it is not the mirror image. `inst-as-immutable` makes
/// a decided record untouchable, so a void that reached one would be refused by
/// `trg_pricing_approval_append_only` — inside the mutation's own transaction,
/// which would roll the mutation back. Every authoring edit to a plan that had
/// ever been approved would fail. The `state = 'submitted'` predicate on the
/// void is what stops that, and this is the test that catches its removal on the
/// backend where the trigger is the PL/pgSQL one.
///
/// The residue — an already-granted approval is not re-invalidated — is the
/// design set's own boundary; see the module doc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker (testcontainers)"]
async fn a_mutation_after_a_committed_approve_commits_and_leaves_the_decision_alone() {
    let pg = Pg::applied().await;
    seed(&pg).await;
    let id = Uuid::from_u128(0xa3);
    submit(&pg, id).await;

    let approvals = ApprovalService::new(DBProvider::<DbError>::new(pg.db().await));
    approvals
        .decide(&scope(), TENANT, approve(id))
        .await
        .expect("the approve commits first");

    let prices = PriceRepo::new(DBProvider::<DbError>::new(pg.db().await));
    prices
        .create_draft(&scope(), TENANT, new_price("us", 0xb_0003))
        .await
        .expect("the mutation must still be possible on an approved plan");

    assert_eq!(state_of(&pg, id).await, ApprovalState::Approved);
}

// ---------------------------------------------------------------------------
// The contention G6 introduced: two units of ONE plan on one audit segment
// ---------------------------------------------------------------------------

/// One approval unit over the seeded plan's revision 0, ready to be opened
/// through a runner the test holds.
///
/// The pin is a literal rather than a re-derivation: nothing below judges
/// content, and a digest computed here would be a second implementation of
/// `content_hash` in a test about sequence numbers.
///
/// **It holds no key, and that is what keeps the next test about the audit
/// segment.** `inst-co-single-pending`'s register enforces one holder per key
/// through `uq_pricing_approval_key_pending`, and the register insert runs
/// *before* the trail append — so two racers holding one key would contend on the
/// register and never reach the segment at all, which is a different property with
/// a different code. That property has its own race,
/// [`two_submits_holding_one_key_contend_on_the_register_and_one_is_refused`], and
/// this one keeps the segment's.
fn new_unit(approval_id: Uuid) -> bss_pricing::infra::storage::repo::approval_repo::NewApproval {
    new_unit_holding(approval_id, BTreeSet::new())
}

/// The same unit, holding `held_keys` — the register's half.
fn new_unit_holding(
    approval_id: Uuid,
    held_keys: BTreeSet<String>,
) -> bss_pricing::infra::storage::repo::approval_repo::NewApproval {
    bss_pricing::infra::storage::repo::approval_repo::NewApproval {
        approval_id,
        tenant_id: TENANT,
        subject_ref: bss_pricing::infra::storage::repo::audit_repo::plan_revision_ref(plan_id(), 0),
        subject_kind: bss_pricing::domain::audit::AuditSubjectKind::PlanRevision,
        content_hash: vec![0x11_u8; 32],
        materiality: json!({ "reason": "noConfiguredThreshold" }),
        held_keys,
    }
}

/// **Two submits over one plan contend on the audit segment, and exactly one
/// wins.**
///
/// The contention this suite did not cover. Its two existing races put a
/// *mutation* against a *decision* on **one** unit, where the contended object
/// is the pending record's row lock. Since the approval plane took audit tokens,
/// `submit` / `approve` / `reject` / `withdraw` each occupy a slot on the
/// **plan's** segment (D-135) — so two acts on two *different* units of the same
/// plan now contend on `(tenant_id, chain_id, seq)`, which they did not before,
/// and no row lock is involved at all.
///
/// It is also the residual race `infra::approval::submit`'s doc claims is
/// "decided by the primary key rather than left open": the single-pending check
/// runs inside the transaction, but under READ COMMITTED two submits can both
/// read before either writes, and neither the approval table's primary key
/// (two distinct `approval_id`s) nor any index on it separates them. The audit
/// chain's head does.
///
/// Driven through `approval_repo::open`, which is the runner-taking form and
/// therefore the only one that can be parked mid-transaction — the same code
/// path `ApprovalService::submit` runs, one layer down. The choreography is this
/// suite's: the first `open` writes and parks, the second is observed **blocked**
/// in `pg_locks` before the first is released, so the two really are in flight.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_submits_over_one_plan_contend_on_the_audit_segment_and_one_is_told_to_retry() {
    let pg = Pg::applied().await;
    seed(&pg).await;

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let first = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            new_unit(Uuid::from_u128(0xa5)),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        // The row and its `submit` record are written and
                        // uncommitted: the segment's next seq is taken.
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let second = {
        let db = pg.db().await;
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            new_unit(Uuid::from_u128(0xa6)),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await
                        .map(|_| ())
                    })
                })
                .await;
            out
        })
    };

    // The second `open` has read the segment head — the same head, because the
    // first has not committed — and its INSERT is waiting on the unique index.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, first)
        .await
        .expect("the first submit must finish once released")
        .expect("its task must not panic")
        .expect("the first submit is uncontended and must commit");

    let loser = tokio::time::timeout(RACE_TIMEOUT, second)
        .await
        .expect("the second submit must be released by the first's commit")
        .expect("its task must not panic")
        .expect_err("two units of one plan cannot both take the same chain position");

    // Told to retry, and told *which* aggregate to retry against (D-159). A
    // driver error here would reach an operator as a 500 about a race whose whole
    // remedy is to re-drive the call.
    match &loser {
        toolkit_db::secure::TxError::Domain(RepoError::ConcurrentMutation { aggregate }) => {
            assert!(
                aggregate.contains(&plan_id().to_string()),
                "the 409 must name the segment that contended: {aggregate}"
            );
        }
        other => panic!("the loser must be a contention, not a storage fault: {other:?}"),
    }

    // And the store holds the winner alone: the loser's approval row rolled back
    // with the record it could not append.
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");
    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            Uuid::from_u128(0xa5)
        )
        .await
        .expect("read the winner")
        .is_some(),
        "the winner's unit must be there"
    );
    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            Uuid::from_u128(0xa6)
        )
        .await
        .expect("read the loser")
        .is_none(),
        "the loser's approval row must have rolled back with its record"
    );
}

// ---------------------------------------------------------------------------
// The register's constraint: `inst-co-single-pending` against two writers
// ---------------------------------------------------------------------------

/// **Two submits holding one key contend on the register, and the loser is
/// refused** — the race the in-transaction check cannot see, and the whole reason
/// the register is a table with an index rather than a comparison.
///
/// # Why this cannot be a `SQLite` suite, and why the check alone is not the rule
///
/// `ApprovalService::submit` reads the register inside the transaction that writes
/// it, and `infra::approval`'s doc used to argue the residual race away: *"the
/// residual race — both reading before either writes, under an isolation level that
/// permits it — is decided by the primary key rather than left open"*. **That
/// premise is false.** `pricing_approval`'s primary key is `approval_id`, which the
/// caller mints — a fresh `Uuid` per request — so two concurrent submits over one
/// plan carry two different primary keys and collide on nothing. Under
/// `READ COMMITTED` both read a free key, both insert, and without
/// `uq_pricing_approval_key_pending` both commit: one canonical scope key held by
/// two approvable always-material units, with the final state decided by whichever
/// commits last.
///
/// `sqlite::memory:` serializes writers, so it can neither confirm nor refute that.
/// This is where the claim is settled.
///
/// # What separates this from the segment race above
///
/// [`two_submits_over_one_plan_contend_on_the_audit_segment_and_one_is_told_to_retry`]
/// uses units holding **no** key, so its loser blocks on `pricing_audit_log`'s
/// unique index and is answered `ConcurrentMutation` — retry, and the retry
/// succeeds. This one's loser is answered **`PENDING_CHANGE_UNIT_EXISTS`**, which
/// is not retriable and must not be: the key is held, and the remedy is to decide
/// or withdraw the unit holding it. Two races, two codes, and the difference is the
/// point — a register that answered `ConcurrentMutation` would send an operator
/// into a retry loop against an invariant.
///
/// The choreography is the file's own: the first `open` parks with its register row
/// written and uncommitted, so the index slot is taken; the second is observed
/// blocked on it before the first is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_submits_holding_one_key_contend_on_the_register_and_one_is_refused() {
    let pg = Pg::applied().await;
    seed(&pg).await;
    let key = scope_key("eu").to_string();

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let first = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        let key = key.clone();
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            new_unit_holding(Uuid::from_u128(0xa7), BTreeSet::from([key])),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        // The register row is written and uncommitted: the partial
                        // index's slot for this key is taken.
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let second = {
        let db = pg.db().await;
        let key = key.clone();
        // A **different** approval id, which is the whole hazard: the primary key
        // the old argument relied on cannot separate two submits, only two attempts
        // at one submit.
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            new_unit_holding(Uuid::from_u128(0xa8), BTreeSet::from([key])),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await
                        .map(|_| ())
                    })
                })
                .await;
            out
        })
    };

    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, first)
        .await
        .expect("the first submit must finish once released")
        .expect("its task must not panic")
        .expect("the first submit is uncontended and must commit");

    let loser = tokio::time::timeout(RACE_TIMEOUT, second)
        .await
        .expect("the second submit must be released by the first's commit")
        .expect("its task must not panic")
        .expect_err("one key cannot be held by two submitted units");

    match &loser {
        toolkit_db::secure::TxError::Domain(RepoError::PendingKeyHeld { key: held }) => {
            assert_eq!(
                *held, key,
                "the refusal must name the key the register refused a second hold on"
            );
        }
        other => panic!("the loser must be the register's refusal, got: {other:?}"),
    }

    // The store holds the winner alone, and the register holds the key once.
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");
    assert_eq!(
        bss_pricing::infra::storage::repo::approval_repo::held_keys_still_pending(
            &conn,
            &scope(),
            TENANT,
            Uuid::from_u128(0xa7)
        )
        .await
        .expect("read the register"),
        vec![key],
        "the winner holds the key"
    );
    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            Uuid::from_u128(0xa8)
        )
        .await
        .expect("read the loser")
        .is_none(),
        "and the loser's whole transaction rolled back - unit, register row and trail"
    );
}

// ---------------------------------------------------------------------------
// The mint guard's classification, measured on the server that renders it
// ---------------------------------------------------------------------------

/// **`uq_pricing_approval_policy_pending`'s loser is a pending-unit conflict on
/// Postgres too, and that is a measurement rather than an inspection** (D-192 clause
/// (2)).
///
/// `infra::storage::policy_guard_or_contention` tells the mint guard from the table's
/// primary key by **reading the driver's message** — the one place in this crate that
/// does — because `sea_orm` exposes no structured constraint identity. Its `SQLite` arm
/// is asserted by `tests/sqlite_approval_repo.rs`'s twin; its **Postgres** arm rested on
/// that server's documented rendering carrying the index name, which is inspection. This
/// executes it. If Postgres ever renders a `unique_violation` without the index name, the
/// classification degrades to `ConcurrentMutation` — safe, but wrong about what the
/// caller should do — and only this case says so.
///
/// # It sits in a race suite and needs no race, deliberately
///
/// D-192's own correction: the state *needs no concurrency to reproduce*, so two
/// sequential `open` calls produce it. What this suite supplies is the machinery — a
/// migrated server and a repository driven over it — and a second Postgres test binary
/// for one case would cost a container per run.
///
/// # And it bypasses the check rather than mocking it
///
/// `infra::approval::open_policy_unit` reads `find_pending_policy_unit` and *then*
/// inserts, so every path through the service refuses a second proposal before the index
/// is consulted: a service-level case stays green with the index dropped. The two writers
/// that can actually race are two calls to `approval_repo::open`, so that is what this
/// drives.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_policy_mint_guard_answers_a_pending_unit_conflict_on_postgres() {
    let pg = Pg::applied().await;
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");

    bss_pricing::infra::storage::repo::approval_repo::open(
        &conn,
        &scope(),
        policy_proposal(Uuid::from_u128(0x_b1), 0),
        stamp_of(SUBMITTER, at(12)),
    )
    .await
    .expect("the first proposal opens its unit");

    let refused = bss_pricing::infra::storage::repo::approval_repo::open(
        &conn,
        &scope(),
        // A different id **and** a different version number — the pair the corrupt state
        // is actually reached through. Nothing here collides on a primary key, and an
        // index keyed on the version number would have admitted exactly this.
        policy_proposal(Uuid::from_u128(0x_b2), 1),
        stamp_of(SUBMITTER, at(12)),
    )
    .await
    .expect_err("the index refuses a second open proposal for one tenant");

    assert_eq!(
        refused,
        RepoError::PendingPolicyUnitHeld {
            tenant_id: TENANT.to_string()
        },
        "on Postgres as on the mirror: the mint guard's loser is told to decide or \
         withdraw the proposal it already holds, not to retry"
    );

    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            Uuid::from_u128(0x_b2)
        )
        .await
        .expect("read the refused unit")
        .is_none(),
        "and nothing landed - a refusal that wrote first would leave two units behind"
    );
}

/// One policy proposal, as `open_policy_unit` builds one.
fn policy_proposal(
    approval_id: Uuid,
    version: u64,
) -> bss_pricing::infra::storage::repo::approval_repo::NewApproval {
    bss_pricing::infra::storage::repo::approval_repo::NewApproval {
        approval_id,
        tenant_id: TENANT,
        subject_ref: version.to_string(),
        subject_kind: bss_pricing::domain::audit::AuditSubjectKind::Policy,
        content_hash: vec![0xc0, 0x1d],
        materiality: json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
        held_keys: BTreeSet::new(),
    }
}

// ---------------------------------------------------------------------------
// The mint guard's POSITION: above the mint, measured on two writers
// ---------------------------------------------------------------------------

/// The two proposals of the race below.
const POLICY_WINNER: Uuid = Uuid::from_u128(0x_b3);
const POLICY_LOSER: Uuid = Uuid::from_u128(0x_b4);

/// The instant both proposals author their version to take effect at.
///
/// One value for both, because a version's `effective_from` is content and two racers
/// disagreeing about it would be a second difference between them — this case is about
/// the one difference it names, their currency sets.
fn policy_effective_from() -> OffsetDateTime {
    at(16)
}

/// The winner's entry set, as the store holds it: **EUR and USD**.
fn winner_rows() -> Vec<ThresholdEntryRow> {
    vec![
        ThresholdEntryRow {
            currency: "EUR".to_owned(),
            absolute_minor: Some(50_000),
            percent_bp: None,
        },
        ThresholdEntryRow {
            currency: "USD".to_owned(),
            absolute_minor: Some(60_000),
            percent_bp: None,
        },
    ]
}

/// The loser's entry set, as the domain carries it: **EUR and GBP**.
///
/// So the two sets **intersect** on EUR and disagree about the rest, which is the pairing
/// the register names and the only one that can tell the two write orders apart. GBP is
/// the row that would have joined the winner's version had both proposals landed, and the
/// assertion that it did not is what makes the refusal a fact about rows rather than only
/// about an error code.
fn loser_entries() -> Vec<ThresholdEntry> {
    vec![
        ThresholdEntry {
            currency: CurrencyCode::new("EUR").expect("three letters"),
            basis: ThresholdBasis::Absolute { minor: 70_000 },
        },
        ThresholdEntry {
            currency: CurrencyCode::new("GBP").expect("three letters"),
            basis: ThresholdBasis::Absolute { minor: 80_000 },
        },
    ]
}

/// **The mint guard stands above the mint it guards, and two proposals whose currency
/// sets intersect are what says so** (D-192, the second of the two things found while
/// implementing clause (2)).
///
/// # What is at stake, and why the intersection is the whole shape
///
/// `ThresholdService::propose` mints its version number off
/// `threshold_repo::latest_version` **inside** its own transaction, so two proposals that
/// both read a store whose greatest version is *n* both mint *n + 1*. Nothing separates
/// them but the order of two writes in one transaction, and that order decides which
/// constraint the loser meets first:
///
/// * **the unit first**, as it stands: the loser's `INSERT` into `pricing_approval` meets
///   `uq_pricing_approval_policy_pending` and is answered `PENDING_CHANGE_UNIT_EXISTS`, a
///   409 naming a remedy the caller can act on;
/// * **the rows first**: the loser's `INSERT` into
///   `pricing_approval_threshold` meets `(tenant, version, currency)` — *but only if the
///   two currency sets intersect* — and that key is not one `infra::storage` classifies,
///   so it arrives as `RepoError::Db` and renders **500**, which is the very shape D-192
///   clause (1) exists about.
///
/// Which is why the two racers here **overlap** rather than coincide or diverge.
/// *Disjoint* sets cannot pin this: with the rows written first they collide on nothing,
/// the loser walks on to the unit insert, and the index refuses it anyway — D-192's own
/// reading, *"an index alone would have delivered the right refusal for disjoint races
/// and a server fault for overlapping ones"*. *Identical* sets **would** pin it, being an
/// intersection, and nothing refuses them earlier — two proposals of one entry set differ
/// in nothing the store compares, `content_hash` carrying no index and the two
/// `approval_id`s being distinct. They would only cost the sharpest assertion below: the
/// loser prices a currency the winner does not, so the winner's committed version can be
/// read back and shown to hold the winner's entry set *and nothing else*.
///
/// # One ordering, and why the sibling races' second direction does not apply here
///
/// [`a_mutation_and_an_approve_in_flight_leave_the_unit_voided_and_the_approve_refused`]
/// asserts both commit orders because its two racers are **different acts** over one
/// record — a mutation and a decision — reaching different constraints and answering
/// asymmetrically, one of the two directions being a deliberate non-refusal. Both racers
/// here are the *same* act. Exchanging which commits first exchanges the labels and
/// nothing else: the winner is whichever committed, and the loser meets the same index
/// over the same currency in the intersection. A second case would be this one with two
/// constants swapped.
///
/// # The choreography, and where the parked transaction parks
///
/// The file's, and step 3 is load-bearing here twice over:
///
/// 1. the winner runs `propose`'s two writes and **parks**, holding both uncommitted —
///    the tenant's one open-proposal slot in `uq_pricing_approval_policy_pending`, and
///    version 0's EUR row;
/// 2. the loser starts; it reads a store with no committed policy, mints 0, and its
///    `INSERT` blocks;
/// 3. a third connection **observes the block**, which is what proves the loser's reads
///    already happened;
/// 4. only then is the winner released, and the loser's insert resolves into the refusal.
///
/// Without step 3 the loser would read the winner's committed rows and mint 1, colliding
/// with nothing — the file's usual reason. The sharper reason is this path's own:
/// `ThresholdState::tag` covers the tenant's **pending** proposal as well as its effective
/// version (D-186), so a loser whose reads happened after the winner committed would be
/// refused by `require_policy_match` with `STALE_VERSION` — before the mint, before the
/// guard, and under a different code entirely. Green, and about the `If-Match` premise
/// rather than about this.
///
/// # The winner is staged and the loser is driven, deliberately
///
/// `propose` owns its transaction and cannot be parked mid-flight, and what has to be held
/// uncommitted is *both* of its writes — the guard's slot **and** the version row the
/// loser collides on — so the winner is composed here out of the same two repository calls
/// `propose` makes. They are written in `propose`'s order, but nothing in this case depends
/// on that: both are uncommitted when the park happens, so the winner's internal order is
/// invisible to the loser. **The loser is the real `ThresholdService::propose`**, and that
/// is where all of this case's sensitivity to the write order lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_proposals_intersecting_on_a_currency_meet_the_mint_guard_and_not_the_version_key() {
    let pg = Pg::applied().await;
    let asserted = AssertedPolicy {
        // Off the one producer of the tag, over a store with no policy in it: this is
        // the premise two operators racing each other actually hold, and the loser's
        // transaction recomputes it before it mints anything.
        tag: ThresholdService::new(DBProvider::<DbError>::new(pg.db().await))
            .state(&scope(), TENANT)
            .await
            .expect("the policy state reads")
            .tag(),
        now: policy_effective_from(),
    };

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let winner = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            policy_proposal(POLICY_WINNER, 0),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        threshold_repo::open_version(
                            txn,
                            &scope(),
                            TENANT,
                            0,
                            policy_effective_from(),
                            &winner_rows(),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        // Uncommitted: the tenant's one open-proposal slot is taken, and
                        // version 0 holds EUR.
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let loser = {
        let db = pg.db().await;
        tokio::spawn(async move {
            ThresholdService::new(DBProvider::<DbError>::new(db))
                .propose(
                    &scope(),
                    TENANT,
                    POLICY_LOSER,
                    policy_effective_from(),
                    loser_entries(),
                    asserted,
                    json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
                    stamp_of(SUBMITTER, at(12)),
                )
                .await
        })
    };

    // The loser has passed its premise check, read `latest_version` as absent and minted
    // 0, and its INSERT is waiting. Only now is the winner allowed to commit.
    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, winner)
        .await
        .expect("the winner must finish once released")
        .expect("its task must not panic")
        .expect("the winner is uncontended and must commit");

    let refusal = tokio::time::timeout(RACE_TIMEOUT, loser)
        .await
        .expect("the loser must be released by the winner's commit")
        .expect("its task must not panic")
        .expect_err("one tenant cannot hold two open policy proposals");

    // **The assertion the case exists for.** With the rows written first this is
    // `DomainError::Internal` off `(tenant, version, currency)` — a 500 for a race whose
    // remedy is to decide or withdraw a proposal.
    //
    // And the tenant in the detail is what says the **index** answered rather than
    // `open_policy_unit`'s in-transaction pre-check, which reached the same code from the
    // same call one statement earlier: the check's rendering names the holding unit and
    // its version, and the index's names the tenant precisely because it cannot name a
    // unit a rollback might unmake.
    match &refusal {
        DomainError::PendingChangeUnitExists(detail) => assert!(
            detail.contains(&TENANT.to_string()),
            "the mint guard must be what answered, not the pre-check: {detail}"
        ),
        other => panic!("the loser must get the typed refusal, not a storage fault: {other:?}"),
    }

    assert_only_the_winners_proposal_landed(&pg).await;
}

/// The winner's proposal, intact and alone, in both stores the act writes to.
///
/// Split out of the case above so that neither the choreography nor the arithmetic has to
/// be read past to reach the other.
async fn assert_only_the_winners_proposal_landed(pg: &Pg) {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");

    // One version number: the loser minted 0 too, and nothing of it survived.
    assert_eq!(
        threshold_repo::latest_version(&conn, &scope(), TENANT)
            .await
            .expect("read the version sequence"),
        Some(0),
        "exactly one proposal minted a version"
    );
    let stored = threshold_repo::read_version(&conn, &scope(), TENANT, 0)
        .await
        .expect("read version 0")
        .expect("the winner's version is there");
    // **The corrupt state D-192 is named for, stated as rows.** The loser's GBP is not in
    // version 0 and the winner's two entries are exactly what is: a version holding the
    // union of two proposals is a row set no approver signed, on a table that then refuses
    // UPDATE and DELETE.
    assert_eq!(
        stored.entries,
        winner_rows(),
        "version 0 must be the winner's entry set and nothing else"
    );
    assert_eq!(stored.effective_from, policy_effective_from());

    // And one unit reviewing it.
    assert_eq!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            POLICY_WINNER
        )
        .await
        .expect("read the winner's unit")
        .expect("the winner's unit is there")
        .state,
        ApprovalState::Submitted
    );
    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            POLICY_LOSER
        )
        .await
        .expect("read the loser's unit")
        .is_none(),
        "the loser's whole transaction rolled back - its unit and its version rows together"
    );
}

// ---------------------------------------------------------------------------
// The same guard's position on the OTHER arm: `retire`
// ---------------------------------------------------------------------------

/// The two retirements of the race below.
const TOMBSTONE_WINNER: Uuid = Uuid::from_u128(0x_b5);
const TOMBSTONE_LOSER: Uuid = Uuid::from_u128(0x_b6);

/// D-192's guard-above-the-mint, pinned on **`retire`** — the arm nothing propagated the
/// `propose` pin to.
///
/// # Why this needs its own case, and why it is the *easier* of the two to reach
///
/// The two arms of D-192 are **independent text**. `ThresholdService::propose` and
/// `ThresholdService::retire` each open their unit before their version rows and each
/// carries its own comment saying so; nothing shares the ordering between them, so the
/// `propose` pin
/// ([`two_proposals_intersecting_on_a_currency_meet_the_mint_guard_and_not_the_version_key`])
/// would stay green with `retire`'s two writes swapped back.
///
/// And the failure is **easier** to reach here, which is the point the register makes:
/// `pricing_approval_threshold_tombstone` is keyed `(tenant, version)` rather than by
/// currency, so a loser meets that key on **any** collision at all — there is no
/// intersecting-currency contrivance to arrange, because two retirements of one tenant
/// always mint the same number and always collide. With the rows written first the loser
/// is answered a bare `RepoError::Db` off that primary key, which renders **500**: a
/// server fault for a race whose whole remedy is to decide or withdraw the retirement the
/// tenant already holds.
///
/// # The choreography is the file's, and step 3 is load-bearing for this path's own reason
///
/// 1. the winner runs `retire`'s two writes and **parks**, holding both uncommitted — the
///    tenant's one open-proposal slot in `uq_pricing_approval_policy_pending`, and version
///    0's tombstone row;
/// 2. the loser starts; it reads a store with no committed policy, mints 0, and its
///    `INSERT` blocks;
/// 3. a third connection **observes the block**, which proves the loser's reads already
///    happened;
/// 4. only then is the winner released, and the loser's insert resolves into the refusal.
///
/// Without step 3 the loser would read the winner's committed rows and mint 1, colliding
/// with nothing — and worse, it would be refused by `require_policy_match` with
/// `STALE_VERSION` first, because `ThresholdState::tag` covers the tenant's pending
/// proposal as well as its effective version (D-186). Green, and about the `If-Match`
/// premise rather than about the guard.
///
/// # The winner is staged and the loser is driven, for the sibling case's reason
///
/// `retire` owns its transaction and cannot be parked mid-flight, and what has to be held
/// uncommitted is *both* of its writes. So the winner is composed here out of the same two
/// calls `retire` makes, in `retire`'s order — and nothing depends on that order, both
/// being uncommitted at the park. **The loser is the real `ThresholdService::retire`**,
/// which is where all of this case's sensitivity to the write order lives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers)"]
async fn two_retirements_meet_the_mint_guard_and_not_the_tombstone_key() {
    let pg = Pg::applied().await;
    let asserted = AssertedPolicy {
        tag: ThresholdService::new(DBProvider::<DbError>::new(pg.db().await))
            .state(&scope(), TENANT)
            .await
            .expect("the policy state reads")
            .tag(),
        now: policy_effective_from(),
    };

    let observer = pg.raw().await;
    let written = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let winner = {
        let db = pg.db().await;
        let (written, release) = (Arc::clone(&written), Arc::clone(&release));
        tokio::spawn(async move {
            let (_db, out) = db
                .in_transaction::<(), RepoError, _>(move |txn| {
                    Box::pin(async move {
                        bss_pricing::infra::storage::repo::approval_repo::open(
                            txn,
                            &scope(),
                            policy_proposal(TOMBSTONE_WINNER, 0),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        threshold_repo::open_tombstone(
                            txn,
                            &scope(),
                            TENANT,
                            0,
                            policy_effective_from(),
                            stamp_of(SUBMITTER, at(12)),
                        )
                        .await?;
                        written.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                })
                .await;
            out
        })
    };

    written.notified().await;

    let loser = {
        let db = pg.db().await;
        tokio::spawn(async move {
            ThresholdService::new(DBProvider::<DbError>::new(db))
                .retire(
                    &scope(),
                    TENANT,
                    TOMBSTONE_LOSER,
                    policy_effective_from(),
                    asserted,
                    json!({ "material": true, "reason": "alwaysMaterialTrigger" }),
                    stamp_of(SUBMITTER, at(12)),
                )
                .await
        })
    };

    pg_support::wait_until_a_backend_blocks(&observer).await;
    release.notify_one();

    tokio::time::timeout(RACE_TIMEOUT, winner)
        .await
        .expect("the winner must finish once released")
        .expect("its task must not panic")
        .expect("the winner is uncontended and must commit");

    let refusal = tokio::time::timeout(RACE_TIMEOUT, loser)
        .await
        .expect("the loser must be released by the winner's commit")
        .expect("its task must not panic")
        .expect_err("one tenant cannot hold two open policy proposals");

    // **The assertion the case exists for.** With the tombstone written first this is
    // `DomainError::Internal` off `(tenant, version)` — a 500 for a race whose remedy is
    // to decide or withdraw the retirement the tenant already holds.
    //
    // The tenant in the detail is what says the **index** answered rather than
    // `open_policy_unit`'s in-transaction pre-check, which reaches the same code one
    // statement earlier: the check names the holding unit and its version, and the index
    // names the tenant precisely because it cannot name a unit a rollback might unmake.
    match &refusal {
        DomainError::PendingChangeUnitExists(detail) => assert!(
            detail.contains(&TENANT.to_string()),
            "the mint guard must be what answered, not the pre-check: {detail}"
        ),
        other => panic!("the loser must get the typed refusal, not a storage fault: {other:?}"),
    }

    assert_only_the_winners_retirement_landed(&pg).await;
}

/// The winner's retirement, intact and alone, in both stores the act writes to.
///
/// Split out for [`assert_only_the_winners_proposal_landed`]'s reason, and it asserts the
/// one thing that case cannot: that the surviving version 0 is a **tombstone** and not an
/// entry version. `latest_version` reads both threshold tables, so a case that only
/// counted versions would pass against a store holding the wrong kind of row at 0.
async fn assert_only_the_winners_retirement_landed(pg: &Pg) {
    let provider = DBProvider::<DbError>::new(pg.db().await);
    let conn = provider.conn().expect("conn");

    assert_eq!(
        threshold_repo::latest_version(&conn, &scope(), TENANT)
            .await
            .expect("read the version sequence"),
        Some(0),
        "exactly one retirement minted a version"
    );
    let stored = threshold_repo::read_version(&conn, &scope(), TENANT, 0)
        .await
        .expect("read version 0")
        .expect("the winner's version is there");
    // A tombstone is a version with **no entries** — `StoredVersion`'s own doc: "empty
    // exactly on a tombstone". Asserted off the entry set rather than a predicate,
    // because the store's row shape is what distinguishes the two tables and the domain's
    // `ThresholdVersion::is_tombstone` is derived from the same emptiness.
    assert!(
        stored.entries.is_empty(),
        "version 0 must be the retirement D-185 declares and not an entry version: {:?}",
        stored.entries
    );
    assert_eq!(stored.effective_from, policy_effective_from());

    assert_eq!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            TOMBSTONE_WINNER
        )
        .await
        .expect("read the winner's unit")
        .expect("the winner's unit is there")
        .state,
        ApprovalState::Submitted
    );
    assert!(
        bss_pricing::infra::storage::repo::approval_repo::read(
            &conn,
            &scope(),
            TENANT,
            TOMBSTONE_LOSER
        )
        .await
        .expect("read the loser's unit")
        .is_none(),
        "the loser's whole transaction rolled back - its unit and its tombstone together"
    );
}
