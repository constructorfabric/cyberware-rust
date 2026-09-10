#![cfg(feature = "postgres")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
//! `PostgreSQL`-backed concurrency suite of the SQL Quota store: the row lock
//! (`FOR UPDATE`) is what makes invariants I6 and I14 hold under concurrent
//! writers, and `SQLite` serializes writers so it cannot show that. Requires
//! Docker. Run with
//! `cargo test -p cf-gears-quota-enforcement-storage-plugin --features postgres --test quota_store_integration_pg`.

use std::sync::Arc;
use std::time::Duration;

use gts::GtsTypeId;
use quota_enforcement_sdk::testing::quota_draft;
use quota_enforcement_sdk::{
    CapPatch, MetricId, PageRequest, Quota, QuotaDraft, QuotaFilter, QuotaId, QuotaPatch,
    QuotaStatus, SubjectRef, TenantId,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sea_orm_migration::MigratorTrait;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use time::OffsetDateTime;
use tokio::sync::oneshot;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::OutboxHandle;
use toolkit_db::secure::{DBRunner, SecureUpdateExt};
use toolkit_db::{ConnectOpts, Db, DbError, connect_db};
use toolkit_security::AccessScope;
use uuid::Uuid;

use quota_enforcement_storage_plugin::infra::storage::Migrator;
use quota_enforcement_storage_plugin::infra::storage::entity::quota_allocation_counter;
use quota_enforcement_storage_plugin::infra::storage::quota_mapping::QuotaUpdate;
use quota_enforcement_storage_plugin::infra::storage::repo::{operation_log_repo, quota_repo};
use quota_enforcement_storage_plugin::{
    Actor, NotificationEnqueuer, QeOutbox, QuotaStore, SqlQuotaStore, StoreError, start_outbox,
};

const USER_PROJECTION: &str = "gts.cf.core.qe.subj.v1~cf.genai.llm_gateway.user.v1~";
const METRIC_TOKENS: &str = "gts.cf.qe.metric.type.v1~cf.qe.metric.ai_tokens_input.v1";

struct PgHarness {
    db: Db,
    store: SqlQuotaStore,
    outbox: OutboxHandle,
    _container: ContainerAsync<Postgres>,
}

fn tenant() -> TenantId {
    TenantId::new(Uuid::from_u128(0x00ac_ce55))
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(tenant().as_uuid())
}

fn actor() -> Actor {
    Actor {
        subject_id: Uuid::from_u128(0x5eed),
        subject_type: None,
    }
}

fn draft(subject_id: &str, cap: Option<u64>) -> QuotaDraft {
    let mut draft = quota_draft(
        SubjectRef {
            projection_type: GtsTypeId::try_new(USER_PROJECTION).expect("type id"),
            subject_id: subject_id.to_owned(),
        },
        cap,
    );
    draft.tenant_id = tenant();
    draft.metric = MetricId::parse(METRIC_TOKENS).expect("metric");
    draft
}

async fn wait_for_tcp(port: u16) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "postgres never listened"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

impl PgHarness {
    async fn up() -> Self {
        let container = test_containers::postgres()
            .with_env_var("POSTGRES_PASSWORD", "pass")
            .with_env_var("POSTGRES_USER", "user")
            .with_env_var("POSTGRES_DB", "app")
            .start()
            .await
            .expect("start postgres");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("mapped port");
        wait_for_tcp(port).await;
        let dsn = format!("postgres://user:pass@127.0.0.1:{port}/app");
        let mut db = None;
        for _ in 0..20 {
            match connect_db(&dsn, ConnectOpts::default()).await {
                Ok(connected) => {
                    db = Some(connected);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
        let db = db.expect("connect to postgres");
        run_migrations_for_testing(&db, Migrator::migrations())
            .await
            .expect("migrations");
        let outbox = start_outbox(db.clone()).await.expect("outbox");
        let enqueuer = Arc::new(QeOutbox::new());
        enqueuer
            .bind(Arc::clone(outbox.outbox()))
            .expect("bind once");
        let enqueuer: Arc<dyn NotificationEnqueuer> = enqueuer;
        let store = SqlQuotaStore::new(db.clone(), enqueuer);
        Self {
            db,
            store,
            outbox,
            _container: container,
        }
    }

    async fn down(self) {
        self.outbox.stop().await;
    }

    async fn create(&self, draft: QuotaDraft) -> QuotaId {
        self.store
            .create_quota(&actor(), &scope(), draft, &[])
            .await
            .expect("created")
    }

    async fn get(&self, id: QuotaId) -> Quota {
        self.store
            .read_quotas(
                &scope(),
                QuotaFilter {
                    ids: vec![id],
                    ..QuotaFilter::default()
                },
                PageRequest::first(1),
            )
            .await
            .expect("read")
            .items
            .pop()
            .expect("present")
    }

    async fn log_len(&self, id: QuotaId) -> usize {
        let conn = self.db.conn().expect("connection");
        operation_log_repo::entries_for_quota(&conn, &AccessScope::allow_all(), id.as_uuid())
            .await
            .expect("log")
            .len()
    }

    /// Lock the Quota row in a transaction of its own, run `body` on it once
    /// the caller says so, then commit. Returns `(locked, release)`: `locked`
    /// resolves once the lock is held, `release` lets the transaction finish.
    fn hold_lock<F>(
        &self,
        id: QuotaId,
        body: F,
    ) -> (
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )
    where
        F: for<'a> FnOnce(
                &'a toolkit_db::DbTx<'a>,
                quota_enforcement_storage_plugin::infra::storage::entity::quota::Model,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<(), DbError>> + Send + 'a>,
            > + Send
            + 'static,
    {
        let (locked_tx, locked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let db = self.db.clone();
        let handle = tokio::spawn(async move {
            db.transaction_ref_mapped(move |tx| {
                Box::pin(async move {
                    let row = quota_repo::find_by_id(tx, &scope(), id.as_uuid(), true)
                        .await
                        .map_err(|e| DbError::Sea(sea_orm::DbErr::Custom(e.to_string())))?
                        .expect("row exists");
                    locked_tx.send(()).expect("test waits");
                    release_rx.await.expect("test releases");
                    body(tx, row).await?;
                    Ok::<(), DbError>(())
                })
            })
            .await
            .expect("holder commits");
        });
        (locked_rx, release_tx, handle)
    }
}

fn scope_err(e: &toolkit_db::secure::ScopeError) -> DbError {
    DbError::Sea(sea_orm::DbErr::Custom(e.to_string()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_patches_that_each_pass_the_pre_check_never_leave_thresholds_on_an_unbounded_cap()
 {
    let h = PgHarness::up().await;
    let id = h.create(draft("u1", Some(100))).await;
    // Both patches are valid against the row as it stands (cap 100, no
    // thresholds); only their merge violates I14.
    let thresholds = h.store.clone();
    let unbind = h.store.clone();
    let (a, b) = tokio::join!(
        async move {
            thresholds
                .update_quota(
                    &actor(),
                    &scope(),
                    id,
                    QuotaPatch {
                        notification_thresholds: Some(vec![50]),
                        ..QuotaPatch::default()
                    },
                    &[],
                )
                .await
        },
        async move {
            unbind
                .update_quota(
                    &actor(),
                    &scope(),
                    id,
                    QuotaPatch {
                        cap: Some(CapPatch::Unbounded),
                        ..QuotaPatch::default()
                    },
                    &[],
                )
                .await
        },
    );
    let outcomes = [a, b];
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "exactly one commits: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Err(StoreError::ThresholdsRequireBoundedCap))),
        "the loser fails I14 inside the lock: {outcomes:?}"
    );
    let row = h.get(id).await;
    assert_eq!(row.record_version, 2);
    assert!(
        row.cap.is_some() || row.notification_thresholds.is_empty(),
        "never thresholds on an unbounded cap: {row:?}"
    );
    assert_eq!(h.log_len(id).await, 2, "the loser wrote nothing");
    h.down().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cap_guard_sees_a_counter_bump_committed_under_the_row_lock() {
    let h = PgHarness::up().await;
    let id = h.create(draft("u1", Some(100))).await;
    let (locked, release, holder) = h.hold_lock(id, move |tx, row| {
        Box::pin(async move {
            let bumped = quota_allocation_counter::Entity::update_many()
                .col_expr(
                    quota_allocation_counter::Column::InFlight,
                    Expr::value(5_i64),
                )
                .filter(quota_allocation_counter::Column::QuotaId.eq(row.id))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .exec(tx)
                .await
                .map_err(|e| scope_err(&e))?;
            assert_eq!(bumped.rows_affected, 1);
            Ok(())
        })
    });
    locked.await.expect("lock held");
    let store = h.store.clone();
    let update = tokio::spawn(async move {
        store
            .update_quota(
                &actor(),
                &scope(),
                id,
                QuotaPatch {
                    cap: Some(CapPatch::Bounded(3)),
                    ..QuotaPatch::default()
                },
                &[],
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!update.is_finished(), "the update waits on FOR UPDATE");
    release.send(()).expect("holder waits");
    holder.await.expect("holder task");
    let err = update
        .await
        .expect("update task")
        .expect_err("below consumed");
    assert_eq!(
        err,
        StoreError::CapBelowConsumed {
            new_cap: 3,
            consumed: 5
        }
    );
    assert_eq!(h.get(id).await.record_version, 1);
    h.down().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_committed_first_lets_a_waiting_deactivation_succeed_with_consecutive_versions() {
    let h = PgHarness::up().await;
    let id = h.create(draft("u1", Some(100))).await;
    let (locked, release, holder) = h.hold_lock(id, move |tx, row| {
        Box::pin(async move {
            let update = QuotaUpdate {
                fail_open_hint: Some(true),
                ..QuotaUpdate::default()
            };
            let applied = quota_repo::apply_update(
                tx,
                &scope(),
                row.id,
                row.record_version,
                &update,
                OffsetDateTime::now_utc(),
            )
            .await
            .map_err(|e| scope_err(&e))?;
            assert!(applied);
            Ok(())
        })
    });
    locked.await.expect("lock held");
    let store = h.store.clone();
    let deactivate =
        tokio::spawn(async move { store.deactivate_quota(&actor(), &scope(), id, &[]).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !deactivate.is_finished(),
        "the deactivation waits on FOR UPDATE"
    );
    release.send(()).expect("holder waits");
    holder.await.expect("holder task");
    deactivate
        .await
        .expect("deactivate task")
        .expect("the deactivation re-reads the committed row and succeeds");
    let row = h.get(id).await;
    assert_eq!(row.status, QuotaStatus::Deactivated);
    assert!(row.fail_open_hint, "the first writer's change survived");
    assert_eq!(
        row.record_version, 3,
        "1, then 2 (update), then 3 (deactivate)"
    );
    h.down().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deactivation_committed_first_makes_a_waiting_update_fail_and_write_nothing() {
    let h = PgHarness::up().await;
    let id = h.create(draft("u1", Some(100))).await;
    let (locked, release, holder) = h.hold_lock(id, move |tx, row| {
        Box::pin(async move {
            let flipped = quota_repo::mark_deactivated(
                tx,
                &scope(),
                row.id,
                row.record_version,
                OffsetDateTime::now_utc(),
            )
            .await
            .map_err(|e| scope_err(&e))?;
            assert!(flipped);
            Ok(())
        })
    });
    locked.await.expect("lock held");
    let store = h.store.clone();
    let update = tokio::spawn(async move {
        store
            .update_quota(
                &actor(),
                &scope(),
                id,
                QuotaPatch {
                    fail_open_hint: Some(true),
                    ..QuotaPatch::default()
                },
                &[],
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!update.is_finished(), "the update waits on FOR UPDATE");
    release.send(()).expect("holder waits");
    holder.await.expect("holder task");
    let err = update
        .await
        .expect("update task")
        .expect_err("the row is deactivated once the lock is released");
    assert_eq!(err, StoreError::QuotaDeactivated { id });
    let row = h.get(id).await;
    assert_eq!(row.status, QuotaStatus::Deactivated);
    assert!(!row.fail_open_hint, "nothing of the update was written");
    assert_eq!(row.record_version, 2);
    assert_eq!(h.log_len(id).await, 1, "only the creation was logged");
    h.down().await;
}

/// The lock-free path also works on `PostgreSQL`: the aggregates the gear's
/// bootstrap and gauges read.
#[tokio::test]
async fn the_platform_plane_reads_answer_on_postgres() {
    let h = PgHarness::up().await;
    h.create(draft("u1", Some(0))).await;
    h.create(draft("u2", None)).await;
    let bindings = h
        .store
        .read_active_projection_bindings()
        .await
        .expect("bindings");
    assert_eq!(bindings.len(), 1);
    let counts = h.store.read_active_quota_counts().await.expect("counts");
    assert_eq!((counts.cap_zero, counts.cap_unbounded), (1, 1));
    assert_eq!(counts.by_metric.values().sum::<u64>(), 2);
    h.down().await;
}

/// Keeps the `DBRunner` import used on every toolchain: the holder closures
/// take the transaction as the runner.
#[allow(dead_code)]
fn _runner_is_a_db_runner(_: &dyn DBRunner) {}
