//! Acceptance against a real database: what one accepted request writes, what a
//! replay returns, and what a reused key with a different request does (T7).
//!
//! Everything here is synchronous. Nothing polls, nothing sleeps, and no worker
//! runs — acceptance is the caller's own task by construction (SPEC §8.1), which
//! is what makes the concurrency case reachable in a plain `#[tokio::test]`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_db::{DBProvider, DbError, DbTx};
use toolkit_gts::gts_id;
use uuid::Uuid;

use types_registry::config::TypesRegistryConfig;
use types_registry::domain::admission::acceptance::{
    AcceptanceContext, AcceptanceError, accept, validate,
};
use types_registry::domain::admission::{
    Candidate, OperationDispatch, Precondition, SubmitRequest,
};
use types_registry::domain::policy::RegistrationPolicy;
// `SubmitRequest` and every row read through a repository carry the domain
// vocabulary; the raw column update below carries the storage one.
use types_registry::domain::enums as domain_enums;
use types_registry::infra::storage::entity::enums as storage_enums;
use types_registry::infra::storage::entity::{entity, operation};
use types_registry::infra::storage::repo::OperationRepo;

mod common;
use common::{TestDir, allow_all, stores, test_db, test_db_file};
use types_registry::domain::ports::metrics::AdmissionMetrics;

const NOW: OffsetDateTime = datetime!(2026-08-18 09:15:30 UTC);
const CF_TYPE: &str = gts_id!("cf.core.example.type.v1~");
const KEY: &str = "idem-key-1";

/// Records every dispatch, and can be made to fail so the rollback is observable.
#[derive(Default)]
struct RecordingDispatch {
    calls: Mutex<Vec<Uuid>>,
    fail: bool,
}

impl RecordingDispatch {
    fn failing() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: true,
        }
    }

    fn calls(&self) -> Vec<Uuid> {
        self.calls.lock().expect("dispatch lock").clone()
    }
}

#[async_trait::async_trait]
impl OperationDispatch for RecordingDispatch {
    async fn enqueue(&self, _tx: &DbTx<'_>, operation_id: Uuid) -> anyhow::Result<()> {
        self.calls.lock().expect("dispatch lock").push(operation_id);
        if self.fail {
            anyhow::bail!("the transport refused this message");
        }
        Ok(())
    }
}

fn schema(gts_id: &str) -> Value {
    json!({
        "$id": format!("gts://{gts_id}"),
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
    })
}

fn request(key: &str, content: Value) -> SubmitRequest {
    SubmitRequest {
        idempotency_key: key.to_owned(),
        kind: domain_enums::OperationKind::Registration,
        dry_run: false,
        candidates: vec![Candidate {
            gts_id: CF_TYPE.to_owned(),
            content: Some(content),
            expected_resource_version: None,
            force: false,
        }],
    }
}

fn batch_request(key: &str, count: usize) -> SubmitRequest {
    let candidates = (1..=count)
        .map(|major| {
            let gts_id = format!("gts.cf.core.example.type.v{major}~");
            Candidate {
                content: Some(schema(&gts_id)),
                gts_id,
                expected_resource_version: None,
                force: false,
            }
        })
        .collect();
    SubmitRequest {
        idempotency_key: key.to_owned(),
        kind: domain_enums::OperationKind::Registration,
        dry_run: false,
        candidates,
    }
}

fn provider(db: &Arc<DBProvider<DbError>>) -> DBProvider<AcceptanceError> {
    DBProvider::new(db.db())
}

fn context<'a>(
    policy: &'a RegistrationPolicy,
    config: &'a TypesRegistryConfig,
    metrics: &'a Arc<dyn AdmissionMetrics>,
) -> AcceptanceContext<'a> {
    AcceptanceContext {
        policy,
        config,
        metrics,
    }
}

// ---------------------------------------------------------------------------
// What one accepted request writes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_accepted_request_writes_one_operation_its_items_and_one_dispatch() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let recorder = Arc::new(RecordingDispatch::default());
    let dispatch: Arc<dyn OperationDispatch> = recorder.clone();

    let accepted = accept(
        &stores(),
        &provider,
        &allow_all(),
        &context(&policy, &config, &common::metrics()),
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("accepted");

    assert!(!accepted.replayed);
    assert!(!accepted.terminal());

    let conn = provider.conn().expect("conn");
    let stored = OperationRepo::find_by_id(&conn, &allow_all(), accepted.operation_id)
        .await
        .expect("read")
        .expect("the operation row");
    assert_eq!(stored.status, domain_enums::OperationStatus::Pending);
    assert_eq!(stored.idempotency_key, KEY);
    assert!(
        stored.tenant_id.is_none(),
        "every P0 operation is platform-plane"
    );
    assert!(stored.started_at.is_none());
    assert!(stored.completed_at.is_none());

    let items = OperationRepo::find_items(&conn, &allow_all(), accepted.operation_id)
        .await
        .expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].gts_id, CF_TYPE);
    assert_eq!(items[0].precondition, Precondition::MustNotExist);
    assert!(
        items[0].request_payload.is_some(),
        "a non-terminal item must carry its payload",
    );

    // The dispatch happened once, inside the same transaction.
    assert_eq!(recorder.calls(), vec![accepted.operation_id]);
}

/// The configured maximum batch crosses the 70-row SQLite-safe insert chunk.
/// Persisting all 100 items proves acceptance splits the multi-row INSERT rather
/// than binding all 1,400 operation-item values in one statement.
#[tokio::test]
async fn maximum_batch_is_inserted_across_sqlite_bind_chunks() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let recorder = Arc::new(RecordingDispatch::default());
    let dispatch: Arc<dyn OperationDispatch> = recorder.clone();
    let request = batch_request("max-batch", config.limits.batch_candidates);
    let expected_ids: Vec<&str> = request
        .candidates
        .iter()
        .map(|candidate| candidate.gts_id.as_str())
        .collect();

    let accepted = accept(
        &stores(),
        &provider,
        &allow_all(),
        &context(&policy, &config, &common::metrics()),
        &dispatch,
        &request,
        NOW,
    )
    .await
    .expect("the configured maximum batch must be accepted");

    let conn = provider.conn().expect("conn");
    let items = OperationRepo::find_items(&conn, &allow_all(), accepted.operation_id)
        .await
        .expect("read operation items");
    assert_eq!(items.len(), config.limits.batch_candidates);
    assert_eq!(
        items
            .iter()
            .map(|item| item.gts_id.as_str())
            .collect::<Vec<_>>(),
        expected_ids,
        "chunking must preserve submission order and every candidate"
    );
    assert_eq!(recorder.calls(), vec![accepted.operation_id]);
}

/// The transaction is one unit: a dispatch failure leaves no operation and no
/// items, so a committed operation is always dispatched and nothing is ever
/// accepted without one.
#[tokio::test]
async fn a_dispatch_failure_rolls_the_whole_acceptance_back() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(RecordingDispatch::failing());

    let err = accept(
        &stores(),
        &provider,
        &allow_all(),
        &context(&policy, &config, &common::metrics()),
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect_err("a dispatch failure must fail the acceptance");
    assert!(matches!(err, AcceptanceError::Dispatch(_)), "got {err}");

    let conn = provider.conn().expect("conn");
    assert!(
        OperationRepo::find_by_idempotency(
            &conn,
            &allow_all(),
            validate(
                &context(&policy, &config, &common::metrics()),
                &request(KEY, schema(CF_TYPE))
            )
            .expect("validated")
            .idempotency_scope_hash
            .as_bytes(),
            KEY,
        )
        .await
        .expect("read")
        .is_none(),
        "the rolled-back transaction must leave no operation",
    );
}

/// A synchronous refusal writes nothing at all — it never reaches a transaction.
#[tokio::test]
async fn a_synchronous_refusal_writes_no_operation() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let recorder = Arc::new(RecordingDispatch::default());
    let dispatch: Arc<dyn OperationDispatch> = recorder.clone();

    let mut refused = request(KEY, schema(gts_id!("acme.crm.customer.type.v1~")));
    refused.candidates[0].gts_id = gts_id!("acme.crm.customer.type.v1~").to_owned();

    let err = accept(
        &stores(),
        &provider,
        &allow_all(),
        &context(&policy, &config, &common::metrics()),
        &dispatch,
        &refused,
        NOW,
    )
    .await
    .expect_err("a closed region must refuse");
    assert!(
        matches!(err, AcceptanceError::PolicyRefused(_)),
        "got {err}"
    );

    let conn = provider.conn().expect("conn");
    let all = operation::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("read operations");
    assert!(all.is_empty(), "a refusal must not write an operation");
    assert!(recorder.calls().is_empty(), "and must not dispatch");
}

// ---------------------------------------------------------------------------
// Replay and conflict
// ---------------------------------------------------------------------------

/// The same request under the same key returns the stored operation, creates no
/// second row, and dispatches nothing further — a redelivery would be the outbox's
/// job, not a second acceptance's.
#[tokio::test]
async fn a_replay_with_a_matching_fingerprint_returns_the_stored_operation() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let recorder = Arc::new(RecordingDispatch::default());
    let dispatch: Arc<dyn OperationDispatch> = recorder.clone();
    let metrics = common::metrics();
    let ctx = context(&policy, &config, &metrics);

    let first = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("first");

    // A differently *spelled* but canonically identical body: key order must not
    // make a replay look like a conflict.
    let reordered = json!({
        "type": "object",
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": format!("gts://{CF_TYPE}"),
    });
    let second = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, reordered),
        NOW,
    )
    .await
    .expect("replay");

    assert_eq!(second.operation_id, first.operation_id);
    assert!(second.replayed);
    assert!(!second.terminal(), "the stored operation is still pending");

    let conn = provider.conn().expect("conn");
    let all = operation::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("read operations");
    assert_eq!(all.len(), 1, "a replay creates no second operation");
    assert_eq!(recorder.calls().len(), 1, "a replay dispatches nothing");
}

/// A replay of a *terminal* operation is reported as terminal, which is what makes
/// the REST layer answer `200` rather than `202` (SPEC §8.1).
#[tokio::test]
async fn a_terminal_replay_reports_terminality() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(RecordingDispatch::default());
    let metrics = common::metrics();
    let ctx = context(&policy, &config, &metrics);

    let first = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("first");

    // Drive the operation terminal the way the worker will. `ck_tr_operation_state`
    // requires both timestamps at `completed`, so this also pins that pairing.
    let conn = provider.conn().expect("conn");
    operation::Entity::update_many()
        .secure()
        .col_expr(
            operation::Column::Status,
            Expr::value(storage_enums::OperationStatus::Completed),
        )
        .col_expr(operation::Column::StartedAt, Expr::value(NOW))
        .col_expr(operation::Column::CompletedAt, Expr::value(NOW))
        .filter(Condition::all().add(operation::Column::Id.eq(first.operation_id)))
        .scope_with(&allow_all())
        .exec(&conn)
        .await
        .expect("complete the operation");

    let replay = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("replay");
    assert_eq!(replay.operation_id, first.operation_id);
    assert!(replay.replayed);
    assert!(replay.terminal());
}

/// A different request under the same key is a conflict, not a replay, and names
/// the operation the key is already bound to.
#[tokio::test]
async fn a_different_fingerprint_under_one_key_is_a_conflict() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(RecordingDispatch::default());
    let metrics = common::metrics();
    let ctx = context(&policy, &config, &metrics);

    let first = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("first");

    let mut different = schema(CF_TYPE);
    different["title"] = json!("a different document");
    let err = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, different),
        NOW,
    )
    .await
    .expect_err("a different request under one key must conflict");
    match err {
        AcceptanceError::FingerprintConflict { operation_id } => {
            assert_eq!(operation_id, first.operation_id);
        }
        other => panic!("expected FingerprintConflict, got {other}"),
    }
}

/// The temporary pre-T20 dry-run refusal happens before idempotency storage, so it
/// cannot reserve a key that a later ordinary submission needs.
#[tokio::test]
async fn a_refused_dry_run_does_not_reserve_the_idempotency_key() {
    let db = test_db().await;
    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let recorder = Arc::new(RecordingDispatch::default());
    let dispatch: Arc<dyn OperationDispatch> = recorder.clone();
    let metrics = common::metrics();
    let ctx = context(&policy, &config, &metrics);

    let mut dry = request(KEY, schema(CF_TYPE));
    dry.dry_run = true;
    let err = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &dry,
        NOW,
    )
    .await
    .expect_err("dry-run is unavailable until T20");
    assert!(matches!(err, AcceptanceError::DryRunNotAccepted));

    let accepted = accept(
        &stores(),
        &provider,
        &allow_all(),
        &ctx,
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("the ordinary request can still use the key");
    assert!(!accepted.replayed);
    assert_eq!(recorder.calls(), vec![accepted.operation_id]);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Concurrent acceptance on one key: exactly one operation row, and every caller
/// comes back with it. The read-then-insert cannot close this window, so the
/// unique constraint is the serialization point and the loser re-reads the winner
/// after verifying the fingerprint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acceptance_on_one_key_yields_one_operation() {
    let dir = TestDir::new("tr-accept");
    let path = dir.path().join("registry.db");
    let db = test_db_file(&path).await;

    let mut handles = Vec::new();
    for _ in 0..8 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let provider = provider(&db);
            let policy = RegistrationPolicy::default();
            let config = TypesRegistryConfig::default();
            let dispatch: Arc<dyn OperationDispatch> = Arc::new(RecordingDispatch::default());
            // SQLite serializes writers with a busy lock rather than row locks, so
            // a contended writer is retried here. On PostgreSQL and MySQL the
            // constraint and the re-read carry the whole protocol.
            for _ in 0..50 {
                match accept(
                    &stores(),
                    &provider,
                    &allow_all(),
                    &context(&policy, &config, &common::metrics()),
                    &dispatch,
                    &request(KEY, schema(CF_TYPE)),
                    NOW,
                )
                .await
                {
                    Ok(accepted) => return Some(accepted.operation_id),
                    Err(e) => assert!(
                        e.to_string().contains("locked"),
                        "unexpected acceptance failure: {e}"
                    ),
                }
            }
            None
        }));
    }

    let mut ids = Vec::new();
    for handle in handles {
        if let Some(id) = handle.await.expect("task") {
            ids.push(id);
        }
    }
    assert_eq!(ids.len(), 8, "every caller must be served");
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "every caller must agree on one operation: {ids:?}",
    );

    let provider = provider(&db);
    let conn = provider.conn().expect("conn");
    let all = operation::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&conn)
        .await
        .expect("read operations");
    assert_eq!(all.len(), 1, "exactly one operation row");
}

// ---------------------------------------------------------------------------
// Acceptance reads no entity state
// ---------------------------------------------------------------------------

/// The criterion is about work, not results, so it is tested by removing the
/// ability to do that work: the `entity` table is dropped out from under
/// acceptance through a second connection to the same file. Any read against it —
/// an existence probe, a family lookup — would now be a hard error, and acceptance
/// still succeeds.
#[tokio::test]
async fn acceptance_reads_no_entity_state() {
    use sea_orm::{ConnectionTrait, Database, Statement};

    let dir = TestDir::new("tr-noentity");
    let path = dir.path().join("registry.db");
    let db = test_db_file(&path).await;

    let raw = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("raw connection");
    raw.execute_raw(Statement::from_string(
        raw.get_database_backend(),
        "DROP TABLE types_registry__entity;".to_owned(),
    ))
    .await
    .expect("drop the entity table");
    // Control: the table really is gone, so a passing test cannot be a false one.
    let probe = entity::Entity::find()
        .secure()
        .scope_with(&allow_all())
        .all(&provider(&db).conn().expect("conn"))
        .await;
    assert!(probe.is_err(), "the control probe must fail");

    let provider = provider(&db);
    let policy = RegistrationPolicy::default();
    let config = TypesRegistryConfig::default();
    let dispatch: Arc<dyn OperationDispatch> = Arc::new(RecordingDispatch::default());
    accept(
        &stores(),
        &provider,
        &allow_all(),
        &context(&policy, &config, &common::metrics()),
        &dispatch,
        &request(KEY, schema(CF_TYPE)),
        NOW,
    )
    .await
    .expect("acceptance must not touch entity state");

    drop(raw);
}
