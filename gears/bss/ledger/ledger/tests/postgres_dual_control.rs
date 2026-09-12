//! Postgres-only repo/service-level tests for the dual-control approval engine
//! (`ApprovalRepo` + `ApprovalService`, VHP-1852). Ignored by default; run with
//! `cargo test -p cf-gears-bss-ledger --test postgres_dual_control -- --ignored`.
//!
//! These are the FAST (testcontainers) counterpart to the cluster-gated e2e
//! `tests/e2e/tests/bss-ledger/test_dual_control.py`. The e2e drives the full
//! REST workflow but with two actors in ONE tenant, so it cannot exercise the
//! cross-tenant isolation of the queue. This suite pins exactly that gap plus the
//! service invariants that have no gear-level coverage:
//!
//! - **approval BOLA** (`approvals_are_invisible_to_a_foreign_tenant_scope`): an
//!   approval seeded for tenant A is unreadable / unlistable under a tenant-B
//!   `AccessScope` — every `ApprovalRepo` read (`read` / `list` / `read_thread` /
//!   `read_active`) resolves to empty, even when asked for tenant A's exact id.
//!   Mirrors `postgres_payments.rs::payment_grains_are_invisible_to_a_foreign_tenant_scope`.
//! - **cross-tenant approve** (`approve_across_a_tenant_boundary_…`): an actor
//!   authenticated in tenant B cannot approve tenant A's approval — it surfaces as
//!   `ApprovalNotFound` and the held mutation NEVER executes (the executor is
//!   never reached). This is the confused-deputy guard the single-tenant e2e
//!   cannot reach.
//! - **preparer != approver** (`self_approval_is_forbidden_…`): the preparer
//!   approving their own request is `SelfApprovalForbidden` and does not execute.
//! - **execute-then-mark** (`approve_by_a_second_actor_executes_…`): a second
//!   actor's approve runs the held mutation exactly once and marks `APPROVED`.
//! - **reject / expiry**: a reject never executes; an expired PENDING is not
//!   actionable.
//!
//! The executor is a `RecordingExecutor` stub (counts `execute` calls) so the
//! lifecycle is tested in isolation from the posting engine — the per-kind replay
//! dispatch lives in `infra/approval/executor.rs` and is exercised by the e2e.

#![allow(
    clippy::non_ascii_literal,
    clippy::let_underscore_must_use,
    clippy::needless_collect,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown,
    clippy::panic,
    clippy::needless_pass_by_value
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bss_ledger::domain::approval::ApprovalKind;
use bss_ledger::domain::approval::intent::{ApprovalIntent, CreditGrantIntent, ReverseIntent};
use bss_ledger::domain::approval::policy::{OperationFacts, resolve_policy};
use bss_ledger::domain::error::DomainError;
use bss_ledger::domain::model::RepoError;
use bss_ledger::domain::ports::metrics::NoopLedgerMetrics;
use bss_ledger::infra::approval::service::{ApprovalExecutor, ApprovalService};
use bss_ledger::infra::storage::migrations::Migrator;
use bss_ledger::infra::storage::repo::{ApprovalRepo, NewPendingApproval};

use sea_orm::{Database, DbErr};
use sea_orm_migration::MigratorTrait;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use time::Duration;
use time::OffsetDateTime;
use toolkit_db::secure::AccessScope;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Lift a component `RepoError` into a `DbError` so a repo write can be the
/// transaction's typed success value (mirrors `postgres_payments.rs`).
fn lift(e: RepoError) -> DbError {
    DbError::Sea(DbErr::Custom(e.to_string()))
}

/// Spin up a Postgres container, migrate the `bss` schema, and return the raw
/// connection + a `bss`-search-path `DBProvider` (mirrors `postgres_payments.rs`).
async fn boot() -> (
    testcontainers_modules::testcontainers::ContainerAsync<Postgres>,
    sea_orm::DatabaseConnection,
    DBProvider<DbError>,
) {
    let container = test_containers::postgres().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let raw = Database::connect(&url).await.unwrap();
    Migrator::up(&raw, None).await.unwrap();

    let repo_url = format!("{url}?options=-c%20search_path%3Dbss,public");
    let tdb = connect_db(&repo_url, ConnectOpts::default()).await.unwrap();
    let provider = DBProvider::<DbError>::new(tdb);
    (container, raw, provider)
}

/// An `ApprovalExecutor` stub that only counts how many times the held mutation
/// was executed — lets a test assert "executed exactly once" vs "never executed"
/// without wiring the real posting engine. Cheap to clone (the counter is shared).
#[derive(Clone, Default)]
struct RecordingExecutor {
    calls: Arc<AtomicUsize>,
}

impl RecordingExecutor {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ApprovalExecutor for RecordingExecutor {
    async fn execute(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        _intent: &ApprovalIntent,
    ) -> Result<(), DomainError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// An authenticated `SecurityContext` for `subject` in `tenant` (mirrors the
/// `authz_tests.rs` builder).
fn ctx_for(subject: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// Seed one PENDING approval (a `REVERSE` intent) for `tenant`, prepared by
/// `prepared_by`, expiring at `expires_at`. Returns its id + the intent (for the
/// `business_key`/`kind` lookups).
async fn seed_pending(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    prepared_by: Uuid,
    expires_at: OffsetDateTime,
) -> (Uuid, ApprovalIntent) {
    let approval_id = Uuid::now_v7();
    let intent = ApprovalIntent::Reverse(ReverseIntent {
        entry_id: Uuid::now_v7(),
        into_period_id: None,
        effective_at: None,
        reason: "dual-control unit".to_owned(),
    });
    let row = NewPendingApproval {
        approval_id,
        tenant,
        kind: intent.kind().as_str().to_owned(),
        business_key: intent.business_key(),
        intent: serde_json::to_value(&intent).expect("serialize approval intent"),
        amount_usd_eq_minor: Some(120_000),
        threshold_snapshot: serde_json::json!({ "d2_threshold_minor": 100_000 }),
        reason_code: "unit-test".to_owned(),
        prepared_by,
        prepared_at: OffsetDateTime::now_utc(),
        correlation_id: Uuid::now_v7(),
        expires_at,
    };
    provider
        .transaction(|txn| {
            let scope = scope.clone();
            let row = row.clone();
            Box::pin(async move {
                ApprovalRepo::insert_pending(txn, &scope, row)
                    .await
                    .map_err(lift)
            })
        })
        .await
        .expect("seed pending approval");
    (approval_id, intent)
}

/// Append one comment to the approval thread (so the BOLA `read_thread` has a row
/// to be denied).
async fn seed_comment(
    provider: &DBProvider<DbError>,
    scope: &AccessScope,
    tenant: Uuid,
    approval_id: Uuid,
    author: Uuid,
) {
    provider
        .transaction(|txn| {
            let scope = scope.clone();
            Box::pin(async move {
                ApprovalRepo::append_comment(
                    txn,
                    &scope,
                    Uuid::now_v7(),
                    approval_id,
                    tenant,
                    0,
                    author,
                    "thread note".to_owned(),
                    OffsetDateTime::now_utc(),
                )
                .await
                .map_err(lift)
            })
        })
        .await
        .expect("seed approval comment");
}

fn in_one_hour() -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::hours(1)
}

/// SQL-level BOLA on the approval queue: an approval (and its comment thread)
/// seeded for tenant A is invisible to a tenant-B `AccessScope` across EVERY
/// read, even when the query names tenant A's exact id. The single-tenant e2e
/// cannot cover this; it is the queue's tenant-isolation guarantee.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn approvals_are_invisible_to_a_foreign_tenant_scope() {
    let (_c, _raw, provider) = boot().await;
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let own = AccessScope::for_tenant(tenant_a);
    let foreign = AccessScope::for_tenant(tenant_b);

    let (approval_id, intent) =
        seed_pending(&provider, &own, tenant_a, preparer, in_one_hour()).await;
    seed_comment(&provider, &own, tenant_a, approval_id, preparer).await;

    let repo = ApprovalRepo::new(provider.clone());

    // Sanity: tenant A's OWN scope sees the row + the thread — so "empty below"
    // means scoped-out, not "the store is simply empty".
    assert!(
        repo.read(&own, tenant_a, approval_id)
            .await
            .expect("read own")
            .is_some(),
        "tenant A's own scope must see its approval"
    );
    assert_eq!(
        repo.read_thread(&own, tenant_a, approval_id)
            .await
            .expect("thread own")
            .len(),
        1,
        "tenant A's own scope must see its comment"
    );

    // Foreign (tenant B) scope: every read resolves to empty/None.
    assert!(
        repo.read(&foreign, tenant_a, approval_id)
            .await
            .expect("read foreign->A")
            .is_none(),
        "a tenant-B scope must NOT read tenant A's approval (SQL-level BOLA)"
    );
    assert!(
        repo.read(&foreign, tenant_b, approval_id)
            .await
            .expect("read foreign->B")
            .is_none(),
        "the approval is not tenant B's either"
    );
    assert!(
        repo.list(&foreign, tenant_a, None, None)
            .await
            .expect("list foreign")
            .is_empty(),
        "a tenant-B scope must list none of tenant A's approvals"
    );
    assert!(
        repo.read_thread(&foreign, tenant_a, approval_id)
            .await
            .expect("thread foreign")
            .is_empty(),
        "a tenant-B scope must not read tenant A's comment thread"
    );
    assert!(
        repo.read_active(
            &foreign,
            tenant_a,
            intent.kind().as_str(),
            &intent.business_key(),
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("read_active foreign")
        .is_none(),
        "a tenant-B scope must not resolve tenant A's active approval"
    );
}

/// A SECOND actor approving a PENDING approval executes the held mutation exactly
/// once and marks the row `APPROVED` with the approver stamped (execute-then-mark).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn approve_by_a_second_actor_executes_and_marks_approved() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let (approval_id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;

    let exec = RecordingExecutor::default();
    let service = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );
    let ctx = ctx_for(approver, tenant);

    service
        .approve(&ctx, &scope, approval_id)
        .await
        .expect("approve");

    assert_eq!(
        exec.calls(),
        1,
        "the held mutation must execute exactly once"
    );
    let row = service
        .get(&ctx, &scope, approval_id)
        .await
        .expect("get")
        .expect("approval present");
    assert_eq!(row.state, "APPROVED");
    assert_eq!(row.approved_by, Some(approver), "the approver is stamped");
}

/// The preparer cannot approve their own pending mutation: `SelfApprovalForbidden`
/// and the held mutation never executes (the row stays PENDING).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn self_approval_is_forbidden_and_does_not_execute() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let (approval_id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;

    let exec = RecordingExecutor::default();
    let service = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );
    // The approver IS the preparer.
    let ctx = ctx_for(preparer, tenant);

    let err = service
        .approve(&ctx, &scope, approval_id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "self-approval must be forbidden, got {err:?}"
    );
    assert_eq!(
        exec.calls(),
        0,
        "self-approval must not execute the mutation"
    );
    let row = service
        .get(&ctx, &scope, approval_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        row.state, "PENDING",
        "the rejected self-approval leaves it pending"
    );
}

/// Confused-deputy guard: an actor authenticated in tenant B cannot approve
/// tenant A's approval — it surfaces as `ApprovalNotFound` (scoped-out), the held
/// mutation never executes, and tenant A's approval is untouched. This is the
/// cross-tenant case the single-tenant e2e cannot reach.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn approve_across_a_tenant_boundary_is_not_found_and_does_not_execute() {
    let (_c, _raw, provider) = boot().await;
    let tenant_a = Uuid::now_v7();
    let tenant_b = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let attacker = Uuid::now_v7();
    let scope_a = AccessScope::for_tenant(tenant_a);
    let scope_b = AccessScope::for_tenant(tenant_b);

    let (approval_id, _) =
        seed_pending(&provider, &scope_a, tenant_a, preparer, in_one_hour()).await;

    let exec = RecordingExecutor::default();
    let service = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );

    // Attacker in tenant B, with tenant B's scope, targets tenant A's approval id.
    let ctx_b = ctx_for(attacker, tenant_b);
    let err = service
        .approve(&ctx_b, &scope_b, approval_id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::ApprovalNotFound(_)),
        "a cross-tenant approve must be NotFound, got {err:?}"
    );
    assert_eq!(
        exec.calls(),
        0,
        "a cross-tenant approve must never execute the mutation"
    );

    // Tenant A's approval is still PENDING and unstamped.
    let ctx_a = ctx_for(preparer, tenant_a);
    let row = service
        .get(&ctx_a, &scope_a, approval_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        row.state, "PENDING",
        "the foreign approval must remain pending"
    );
    assert_eq!(row.approved_by, None);
}

/// A reject marks the approval `REJECTED` and never executes the held mutation.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reject_marks_rejected_without_executing() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    let (approval_id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;

    let exec = RecordingExecutor::default();
    let service = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );
    let ctx = ctx_for(approver, tenant);

    service
        .reject(&ctx, &scope, approval_id, "not this period".to_owned())
        .await
        .expect("reject");

    assert_eq!(exec.calls(), 0, "a reject must never execute the mutation");
    let row = service
        .get(&ctx, &scope, approval_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "REJECTED");
}

/// An approval past its TTL is not actionable: an approve attempt fails
/// `ApprovalNotActionable` and never executes.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_expired_pending_is_not_actionable() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);

    // Already expired an hour ago.
    let (approval_id, _) = seed_pending(
        &provider,
        &scope,
        tenant,
        preparer,
        OffsetDateTime::now_utc() - Duration::hours(1),
    )
    .await;

    let exec = RecordingExecutor::default();
    let service = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );
    let ctx = ctx_for(approver, tenant);

    let err = service
        .approve(&ctx, &scope, approval_id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::ApprovalNotActionable(_)),
        "an expired approval must not be actionable, got {err:?}"
    );
    assert_eq!(exec.calls(), 0, "an expired approval must never execute");
}

// ─── service lifecycle: gate / list / request-changes / resubmit / cancel / comments ───

/// A `CreditGrant` intent keyed by `app_id`, sized at `amount` minor (the
/// amount-gated kind the `gate` threshold check reads).
fn credit_grant_intent(app_id: &str, amount: i64) -> ApprovalIntent {
    ApprovalIntent::CreditGrant(CreditGrantIntent {
        tenant_id: Uuid::now_v7(),
        payer_tenant_id: Uuid::now_v7(),
        credit_application_id: app_id.to_owned(),
        currency: "USD".to_owned(),
        amount_minor: amount,
        credit_grant_event_type: Some("promo".to_owned()),
    })
}

/// A fresh `Reverse` intent (same kind as `seed_pending`'s) — resubmit requires
/// the edited intent's kind to match the stored row.
fn reverse_intent() -> ApprovalIntent {
    ApprovalIntent::Reverse(ReverseIntent {
        entry_id: Uuid::now_v7(),
        into_period_id: None,
        effective_at: None,
        reason: "resubmitted".to_owned(),
    })
}

/// Build an `ApprovalService` over `provider` with the recording stub executor.
fn service(provider: &DBProvider<DbError>, exec: &RecordingExecutor) -> ApprovalService {
    ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    )
}

/// `gate` over the default D2 threshold ($1000 = 100_000 minor; no tenant policy
/// row ⇒ ratified defaults) creates a PENDING approval and returns its id; a
/// second gate on the same `(tenant, kind, business_key)` returns the SAME id
/// (DC13 active-uniqueness), never a duplicate.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn gate_over_threshold_creates_pending_and_is_idempotent() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let exec = RecordingExecutor::default();
    let svc = service(&provider, &exec);
    let ctx = ctx_for(Uuid::now_v7(), tenant);

    let intent = credit_grant_intent("CA-DC13", 120_000);
    let facts = OperationFacts {
        kind: ApprovalKind::CreditGrant,
        amount_usd_eq_minor: Some(120_000),
        effective_at: None,
        has_outstanding_balance: false,
    };
    let id1 = svc
        .gate(
            &ctx,
            &scope,
            intent.clone(),
            facts,
            "credit-grant".to_owned(),
        )
        .await
        .expect("gate")
        .expect("over-threshold gate must create a pending approval");
    let id2 = svc
        .gate(&ctx, &scope, intent, facts, "credit-grant".to_owned())
        .await
        .expect("gate (replay)")
        .expect("replay returns the active approval");
    assert_eq!(
        id1, id2,
        "DC13: one active approval per (tenant, kind, business_key)"
    );

    let pending = svc
        .list(&ctx, &scope, Some("PENDING"), Some("CREDIT_GRANT"))
        .await
        .expect("list");
    assert_eq!(pending.len(), 1, "exactly one PENDING, got {pending:?}");
    assert_eq!(exec.calls(), 0, "gating never executes the mutation");
}

/// `gate` below the D2 threshold returns `None` — the caller proceeds single-actor
/// and no approval row is created.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn gate_below_threshold_creates_no_approval() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let svc = service(&provider, &RecordingExecutor::default());
    let ctx = ctx_for(Uuid::now_v7(), tenant);

    let out = svc
        .gate(
            &ctx,
            &scope,
            credit_grant_intent("CA-SMALL", 50_000),
            OperationFacts {
                kind: ApprovalKind::CreditGrant,
                amount_usd_eq_minor: Some(50_000),
                effective_at: None,
                has_outstanding_balance: false,
            },
            "credit-grant".to_owned(),
        )
        .await
        .expect("gate");
    assert!(
        out.is_none(),
        "below-threshold gate must not require approval"
    );
    assert!(
        svc.list(&ctx, &scope, None, None)
            .await
            .expect("list")
            .is_empty(),
        "no approval row is created below threshold"
    );
}

/// request-changes (by a second actor) parks a PENDING approval NEEDS_REWORK; the
/// preparer then resubmits an edited same-kind intent and it returns to PENDING
/// with a bumped revision.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn request_changes_then_resubmit_recycles_to_pending() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, seeded) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());

    svc.request_changes(
        &ctx_for(approver, tenant),
        &scope,
        id,
        "add a memo".to_owned(),
    )
    .await
    .expect("request_changes");
    let row = svc
        .get(&ctx_for(approver, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "NEEDS_REWORK");

    // Resubmit the SAME target (only the amount may be edited, and Reverse carries
    // none), so the identity-pin admits it and the row recycles.
    svc.resubmit(&ctx_for(preparer, tenant), &scope, id, seeded)
        .await
        .expect("resubmit");
    let row = svc
        .get(&ctx_for(preparer, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "PENDING");
    assert!(
        row.revision >= 1,
        "resubmit bumps the revision, got {}",
        row.revision
    );
}

/// DC #1: a resubmit that SWAPS the target identity (same kind, but a
/// fresh `entry_id` — a different recipient/target than the seeded intent) is
/// rejected and the row stays NEEDS_REWORK. Without the identity-pin a preparer
/// could, after a request-changes, redirect an over-threshold approval to a swapped
/// party; the approver — who never sees the target in `ApprovalDto` — would then
/// execute the swapped intent the executor replays from storage.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn resubmit_with_swapped_target_is_rejected() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _seeded) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());

    svc.request_changes(&ctx_for(approver, tenant), &scope, id, "redo".to_owned())
        .await
        .expect("request_changes");

    // `reverse_intent()` is the SAME kind (Reverse) but a fresh `entry_id` — a
    // swapped target. The kind guard passes; the identity-pin must reject it.
    let err = svc
        .resubmit(&ctx_for(preparer, tenant), &scope, id, reverse_intent())
        .await
        .expect_err("a swapped-target resubmit must be rejected");
    assert!(
        matches!(err, DomainError::ApprovalNotActionable(_)),
        "expected ApprovalNotActionable, got {err:?}"
    );
    let row = svc
        .get(&ctx_for(preparer, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        row.state, "NEEDS_REWORK",
        "a rejected resubmit leaves the row in NEEDS_REWORK"
    );
}

/// request-changes by the preparer themselves is forbidden (decider != preparer).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn request_changes_by_preparer_is_forbidden() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());

    let err = svc
        .request_changes(&ctx_for(preparer, tenant), &scope, id, "x".to_owned())
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::SelfApprovalForbidden(_)),
        "got {err:?}"
    );
}

/// resubmit by a non-preparer is rejected (only the preparer may resubmit).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn resubmit_by_non_preparer_is_rejected() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());
    svc.request_changes(&ctx_for(approver, tenant), &scope, id, "rework".to_owned())
        .await
        .expect("request_changes");

    let err = svc
        .resubmit(
            &ctx_for(Uuid::now_v7(), tenant),
            &scope,
            id,
            reverse_intent(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::ApprovalNotActionable(_)),
        "got {err:?}"
    );
}

/// The preparer cancels their own active approval (-> CANCELLED); a non-preparer
/// cannot (it stays active).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cancel_is_preparer_only() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());

    let err = svc
        .cancel(&ctx_for(Uuid::now_v7(), tenant), &scope, id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::ApprovalNotActionable(_)),
        "got {err:?}"
    );
    let row = svc
        .get(&ctx_for(preparer, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "PENDING", "a failed cancel leaves it active");

    svc.cancel(&ctx_for(preparer, tenant), &scope, id)
        .await
        .expect("cancel");
    let row = svc
        .get(&ctx_for(preparer, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "CANCELLED");
}

/// add_comment appends to the append-only thread (no state change); thread reads
/// it back.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn add_comment_appends_to_the_thread() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;
    let svc = service(&provider, &RecordingExecutor::default());
    let ctx = ctx_for(Uuid::now_v7(), tenant);

    svc.add_comment(&ctx, &scope, id, "please clarify the period".to_owned())
        .await
        .expect("add_comment");
    let thread = svc.thread(&ctx, &scope, id).await.expect("thread");
    assert!(
        thread.iter().any(|c| c.body == "please clarify the period"),
        "the appended comment must appear in the thread, got {thread:?}"
    );
    let row = svc
        .get(&ctx, &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "PENDING", "a comment is not a state change");
}

/// An `ApprovalExecutor` that BLOCKS in `execute` until released — lets a test
/// interpose a concurrent decision while an approve is mid-execute (the row
/// latched `APPROVING`). `entered` fires when execute is reached; `gate` releases
/// it; `calls` counts executions (like `RecordingExecutor`).
#[derive(Clone)]
struct BlockingExecutor {
    entered: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ApprovalExecutor for BlockingExecutor {
    async fn execute(
        &self,
        _ctx: &SecurityContext,
        _scope: &AccessScope,
        _intent: &ApprovalIntent,
    ) -> Result<(), DomainError> {
        self.entered.notify_one();
        self.gate.notified().await;
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// H2: once an approve latches `PENDING → APPROVING` and begins executing, a
/// concurrent `reject` can no longer win (the decision verbs are keyed on
/// `PENDING`) — so the governed mutation never ends up committed against a
/// REJECTED approval. The approve then completes `APPROVED`, executing exactly
/// once.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reject_cannot_win_after_approve_latches_approving() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let preparer = Uuid::now_v7();
    let approver = Uuid::now_v7();
    let rejecter = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let (id, _) = seed_pending(&provider, &scope, tenant, preparer, in_one_hour()).await;

    let exec = BlockingExecutor {
        entered: Arc::new(tokio::sync::Notify::new()),
        gate: Arc::new(tokio::sync::Notify::new()),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let svc = ApprovalService::new(
        provider.clone(),
        Arc::new(exec.clone()),
        Arc::new(NoopLedgerMetrics),
        bss_ledger::config::FxConfig::default(),
    );

    // Spawn approve: it latches PENDING→APPROVING (own txn, committed), then blocks
    // inside execute.
    let svc_a = svc.clone();
    let ctx_a = ctx_for(approver, tenant);
    let scope_a = scope.clone();
    let approve_task = tokio::spawn(async move { svc_a.approve(&ctx_a, &scope_a, id).await });

    // Wait until approve is inside execute — the row is now APPROVING (committed).
    exec.entered.notified().await;
    let row = svc
        .get(&ctx_for(approver, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        row.state, "APPROVING",
        "approve must latch APPROVING before executing"
    );

    // A concurrent reject must NOT win — the row is no longer PENDING.
    let reject_err = svc
        .reject(
            &ctx_for(rejecter, tenant),
            &scope,
            id,
            "too late".to_owned(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(reject_err, DomainError::ApprovalNotActionable(_)),
        "reject after the APPROVING latch must fail, got {reject_err:?}"
    );

    // Release execute → approve completes APPROVED, having executed exactly once.
    exec.gate.notify_one();
    approve_task
        .await
        .expect("approve task joins")
        .expect("approve completes");
    let row = svc
        .get(&ctx_for(approver, tenant), &scope, id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(row.state, "APPROVED", "approve completes APPROVED");
    assert_eq!(
        exec.calls.load(Ordering::SeqCst),
        1,
        "the governed mutation executed exactly once"
    );
}

/// Z11-2 / DC8: `set_policy` writes an effective-dated threshold version that the
/// resolver then picks up; a second write supersedes (version increments).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn set_policy_persists_and_resolves() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let admin = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let svc = service(&provider, &RecordingExecutor::default());
    let repo = ApprovalRepo::new(provider.clone());

    // No row yet → the resolver yields the ratified defaults (d2 = 100_000).
    assert!(
        repo.read_policy_versions(&scope, tenant)
            .await
            .expect("read")
            .is_empty()
    );

    let v0 = svc
        .set_policy(
            &ctx_for(admin, tenant),
            &scope,
            250_000,
            10,
            3600,
            OffsetDateTime::now_utc() - Duration::hours(1),
        )
        .await
        .expect("set policy");
    assert_eq!(v0, 0, "first version is 0");

    let versions = repo
        .read_policy_versions(&scope, tenant)
        .await
        .expect("read");
    let policy = resolve_policy(&versions, OffsetDateTime::now_utc());
    assert_eq!(
        policy.d2_threshold_minor, 250_000,
        "resolver picks the written threshold"
    );
    assert_eq!(policy.a6_backdating_biz_days, 10);
    assert_eq!(policy.pending_ttl_seconds, 3600);

    // A second write supersedes (version increments; resolver picks the latest).
    let v1 = svc
        .set_policy(
            &ctx_for(admin, tenant),
            &scope,
            500_000,
            5,
            7200,
            OffsetDateTime::now_utc(),
        )
        .await
        .expect("set policy 2");
    assert_eq!(v1, 1, "second version is 1");
    let versions = repo
        .read_policy_versions(&scope, tenant)
        .await
        .expect("read");
    assert_eq!(
        resolve_policy(&versions, OffsetDateTime::now_utc()).d2_threshold_minor,
        500_000
    );
}

/// Z11-2 / DC9: an out-of-range threshold is REJECTED (no clamp) and writes no row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn set_policy_rejects_out_of_range() {
    let (_c, _raw, provider) = boot().await;
    let tenant = Uuid::now_v7();
    let admin = Uuid::now_v7();
    let scope = AccessScope::for_tenant(tenant);
    let svc = service(&provider, &RecordingExecutor::default());

    // d2 below the floor (10_000 minor = 100 USD) → rejected.
    let err = svc
        .set_policy(
            &ctx_for(admin, tenant),
            &scope,
            5_000,
            5,
            7200,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::DualControlPolicyOutOfRange(_)),
        "out-of-range d2 must be rejected, got {err:?}"
    );
    assert!(
        ApprovalRepo::new(provider.clone())
            .read_policy_versions(&scope, tenant)
            .await
            .expect("read")
            .is_empty(),
        "a rejected config writes no row"
    );
}
