//! The one-transaction contract, over a real store.
//!
//! `idempotency_repo`'s module doc rests everything on a sentence: `claim` and
//! `record_response` "MUST run inside the same transaction as the mutation they
//! guard", because split across two "the claim commits while the mutation does
//! not, and every retry of a request that never happened is answered 'already
//! done', forever." Until this suite that was an argument. Here it is an
//! observation: a mutation that fails inside the seam leaves **no claim row**,
//! so the retry claims afresh.
//!
//! Everything is asserted against rows rather than against return values where
//! the two could disagree — a replay that quietly re-ran its mutation would
//! still return the stored body, and only the row count says it did not.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use bss_pricing::domain::error::DomainError;
use bss_pricing::domain::plan::PlanRevision;
use bss_pricing::domain::plan_shape::BillingCycle;
use bss_pricing::domain::scope_key::PlanId;
use bss_pricing::infra::idempotent::{Guarded, GuardedRequest, TxFuture, guarded};
use bss_pricing::infra::storage::entity::{idempotency_dedup, plan};
use bss_pricing::infra::storage::migrations::Migrator;
use bss_pricing::infra::storage::repo::{IdempotencyGate, NewPlanDraft, plan_repo};

use bss_pricing::domain::instant::utc_ymd_hms;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use sea_orm_migration::MigratorTrait;
use serde_json::{Value as JsonValue, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, DbTx, SecureEntityExt};
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use uuid::Uuid;

/// One value for a whole test binary: these suites drive a repository or a
/// service directly, where the value the HTTP edge would have established has
/// no producer. What each suite asserts *about* it is stated where it asserts
/// it.
const TEST_CORRELATION: uuid::Uuid = uuid::Uuid::from_u128(0x_c0_11_a7_10);

/// The operation name the two guarded creates are scoped under; a second one
/// below proves the scoping is real.
const CREATE_PLAN: &str = "bss_pricing.create_plan";
const CREATE_PRICE: &str = "bss_pricing.create_price";

async fn provider() -> DBProvider<DbError> {
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

fn new_draft(plan_id: PlanId, tenant_id: Uuid) -> NewPlanDraft {
    NewPlanDraft {
        plan_name: None,
        plan_id,
        tenant_id,
        created_by: Uuid::from_u128(0xac_11),
        created_at_utc: at(9),
        sku_id: None,
        plan_tier: Some("gold".to_owned()),
        billing_cycle: Some(BillingCycle::Recurring),
        frequency: None,
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

fn request(operation: &'static str, key: &str, hash: &[u8], tenant: Uuid) -> GuardedRequest {
    GuardedRequest {
        operation,
        client_key: key.to_owned(),
        request_hash: hash.to_vec(),
        tenant_id: tenant,
        status: 201,
        now: at(9),
    }
}

/// The response body a handler would send — recorded, and replayed verbatim.
///
/// Fallible because the seam's renderer is: a handler's `serde_json::to_value`
/// over its view type can fail, and swallowing that would record a body nobody
/// was sent. Here nothing can fail, and the `Ok` is the seam's signature.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the seam's renderer contract is fallible; a test double that was not would not type-check against it"
)]
fn render(revision: &PlanRevision) -> Result<JsonValue, DomainError> {
    Ok(json!({ "plan_id": revision.plan_id.to_string(), "revision": revision.revision }))
}

async fn claim_rows(db: &DBProvider<DbError>, tenant: Uuid) -> usize {
    let conn = db.conn().expect("conn");
    idempotency_dedup::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .filter(Condition::all().add(idempotency_dedup::Column::TenantId.eq(tenant)))
        .all(&conn)
        .await
        .expect("count claims")
        .len()
}

async fn plan_rows(db: &DBProvider<DbError>, tenant: Uuid) -> usize {
    let conn = db.conn().expect("conn");
    plan::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant))
        .filter(Condition::all().add(plan::Column::TenantId.eq(tenant)))
        .all(&conn)
        .await
        .expect("count plans")
        .len()
}

#[tokio::test]
async fn a_second_identical_request_replays_the_stored_answer_and_runs_nothing() {
    let db = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let gate = IdempotencyGate::new(Duration::from_hours(1));
    let plan_id = PlanId::new(Uuid::now_v7());
    let runs = Arc::new(AtomicUsize::new(0));

    let first = {
        let draft = new_draft(plan_id, tenant);
        let scope_for_body = scope.clone();
        let runs = Arc::clone(&runs);
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-1", b"digest-1", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("the first request performs")
    };
    let Guarded::Performed(revision) = first else {
        panic!("a free key performs, it does not replay");
    };
    assert_eq!(revision.revision, 0);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // The same key, the same digest, and a mutation that would create a SECOND
    // plan if it ran at all.
    let second_plan = PlanId::new(Uuid::now_v7());
    let replay = {
        let draft = new_draft(second_plan, tenant);
        let scope_for_body = scope.clone();
        let runs = Arc::clone(&runs);
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-1", b"digest-1", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("the second request replays")
    };

    match replay {
        Guarded::Replayed { status, body } => {
            assert_eq!(status, 201, "the replay answers the status that was sent");
            assert_eq!(
                body,
                render(&revision).expect("render"),
                "the recorded body is the body the FIRST caller received, id and all"
            );
        }
        Guarded::Performed(_) => panic!("an answered key must not run its mutation again"),
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a replay reaches the mutation zero times, or at-most-once is a claim about nothing"
    );
    assert_eq!(plan_rows(&db, tenant).await, 1);
    assert_eq!(claim_rows(&db, tenant).await, 1);
}

#[tokio::test]
async fn the_same_key_with_a_different_payload_is_refused_and_changes_nothing() {
    let db = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let gate = IdempotencyGate::new(Duration::from_hours(1));
    let first_plan = PlanId::new(Uuid::now_v7());

    {
        let draft = new_draft(first_plan, tenant);
        let scope_for_body = scope.clone();
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-2", b"digest-a", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("the first request performs");
    }

    let other_plan = PlanId::new(Uuid::now_v7());
    let refusal = {
        let draft = new_draft(other_plan, tenant);
        let scope_for_body = scope.clone();
        guarded::<PlanRevision, _, _>(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-2", b"digest-b", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect_err("one key, two requests: neither replayed nor re-executed")
    };

    assert!(
        matches!(refusal, DomainError::IdempotencyPayloadMismatch(_)),
        "{refusal:?}"
    );
    assert_eq!(
        plan_rows(&db, tenant).await,
        1,
        "the mismatching request must not have run"
    );
}

#[tokio::test]
async fn a_failed_mutation_leaves_no_claim_behind_so_the_retry_claims_afresh() {
    // The whole reason the seam exists. Under two transactions the claim would
    // commit while the mutation rolled back, and every later retry of a request
    // that never happened would be answered "already done" - forever, since
    // nothing in the gear reaps a claim before its TTL.
    let db = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let gate = IdempotencyGate::new(Duration::from_hours(1));
    let plan_id = PlanId::new(Uuid::now_v7());

    // The mutation fails: an availability bound below the millisecond quantum is
    // refused by the repository (D-144), after the claim row is written.
    let doomed = {
        let mut draft = new_draft(plan_id, tenant);
        draft.available_from = Some(at(9) + time::Duration::microseconds(1));
        let scope_for_body = scope.clone();
        guarded::<PlanRevision, _, _>(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-3", b"digest-3", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect_err("the mutation is refused")
    };
    assert!(
        matches!(doomed, DomainError::TimestampPrecisionExceeded(_)),
        "the refusal survives the rollback as itself: {doomed:?}"
    );
    assert_eq!(
        claim_rows(&db, tenant).await,
        0,
        "the claim rolled back with the mutation it guards"
    );

    // The retry of the very same key now works, which is the observable form of
    // "a request that never happened is not remembered as one that did".
    let retried = {
        let draft = new_draft(plan_id, tenant);
        let scope_for_body = scope.clone();
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-3", b"digest-3", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("the retry claims afresh")
    };
    assert!(matches!(retried, Guarded::Performed(_)));
    assert_eq!(plan_rows(&db, tenant).await, 1);
}

#[tokio::test]
async fn one_client_key_on_two_operations_does_not_collide() {
    // `operation` is part of the composite key, so a client that reuses one
    // request id across the two guarded creates gets both, not a replay of the
    // first as the answer to the second.
    let db = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let gate = IdempotencyGate::new(Duration::from_hours(1));

    for (operation, plan_id) in [
        (CREATE_PLAN, PlanId::new(Uuid::now_v7())),
        (CREATE_PRICE, PlanId::new(Uuid::now_v7())),
    ] {
        let draft = new_draft(plan_id, tenant);
        let scope_for_body = scope.clone();
        let outcome = guarded(
            &db,
            &gate,
            &scope,
            request(operation, "shared-key", b"digest-4", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("each operation holds the key independently");
        assert!(
            matches!(outcome, Guarded::Performed(_)),
            "{operation} was answered as a replay of the other operation"
        );
    }

    assert_eq!(plan_rows(&db, tenant).await, 2);
    assert_eq!(claim_rows(&db, tenant).await, 2);
}

#[tokio::test]
async fn a_replay_is_answered_from_the_store_and_not_from_a_second_render() {
    // The renderer is the caller's, and the recorded body is what the FIRST
    // caller was sent. A replay that re-rendered would answer the second caller
    // something the first never received - which is the only thing the stored
    // body is for.
    let db = provider().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let gate = IdempotencyGate::new(Duration::from_hours(1));
    let plan_id = PlanId::new(Uuid::now_v7());

    {
        let draft = new_draft(plan_id, tenant);
        let scope_for_body = scope.clone();
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-5", b"digest-5", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            render,
        )
        .await
        .expect("performed");
    }

    let replay = {
        let draft = new_draft(PlanId::new(Uuid::now_v7()), tenant);
        let scope_for_body = scope.clone();
        guarded(
            &db,
            &gate,
            &scope,
            request(CREATE_PLAN, "key-5", b"digest-5", tenant),
            move |txn: &DbTx<'_>| -> TxFuture<'_, PlanRevision> {
                Box::pin(async move {
                    plan_repo::create_draft_on(txn, &scope_for_body, draft)
                        .await
                        .map_err(|e| bss_pricing::infra::storage::repo_failure(&e))
                })
            },
            // A renderer that would answer something else entirely, to prove it
            // is never reached on the replay path.
            |_| Ok(json!({ "plan_id": "a body nobody was ever sent" })),
        )
        .await
        .expect("replayed")
    };

    match replay {
        Guarded::Replayed { body, .. } => assert_eq!(
            body["plan_id"],
            json!(plan_id.to_string()),
            "the replay carries the ORIGINAL id, which is why the body is stored at all"
        ),
        Guarded::Performed(_) => panic!("an answered key must replay"),
    }
}
