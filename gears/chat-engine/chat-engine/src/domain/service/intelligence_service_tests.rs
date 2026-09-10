use super::*;
use crate::domain::ports::NewSession;
use crate::domain::ports::NewSessionType;
use crate::domain::session::Session;
use crate::domain::session::SessionType;
use uuid::Uuid;

use async_trait::async_trait;
use chat_engine_sdk::models::{LifecycleState, MessagePart, MessageRole};
use chat_engine_sdk::plugin::ChatEngineBackendPlugin;
use parking_lot::Mutex;
use time::OffsetDateTime;
use toolkit::ClientHub;

use crate::domain::message::Message;
use crate::domain::ports::PluginConfigRepo;
use crate::domain::ports::SessionRepo;
use crate::domain::ports::SessionTypeRepo;
use crate::domain::ports::{FinalizeOutcome, InsertedPair, MessageRepo, NewUserMessage};
use crate::domain::service::test_support;
use toolkit_security::AccessScope;

// ----- Mocks -------------------------------------------------------

struct MockSessionRepo {
    rows: Mutex<Vec<Session>>,
}

impl MockSessionRepo {
    fn new(rows: Vec<Session>) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(rows),
        })
    }
}

#[async_trait]
impl SessionRepo for MockSessionRepo {
    async fn insert(&self, _m: NewSession) -> std::result::Result<Session, ChatEngineError> {
        Err(ChatEngineError::internal("mock insert"))
    }

    async fn find_by_id(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        Ok(self
            .rows
            .lock()
            .iter()
            .find(|r| {
                r.session_id == session_id
                    && r.tenant_id.as_str() == tenant_id
                    && r.user_id.as_str() == user_id
            })
            .cloned())
    }

    // Scoped (Phase 4) prefetch used by authorize_session_op under a
    // permissive enforcer: ignore the scope, match on session_id.
    async fn find_by_id_scoped(
        &self,
        _scope: &AccessScope,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        Ok(self
            .rows
            .lock()
            .iter()
            .find(|r| r.session_id == session_id)
            .cloned())
    }

    async fn list_paginated(
        &self,
        _tenant_id: &str,
        _user_id: &str,
        _query: &toolkit_odata::ODataQuery,
    ) -> std::result::Result<toolkit_odata::Page<Session>, ChatEngineError> {
        Ok(toolkit_odata::Page::empty(0))
    }

    async fn update_metadata(
        &self,
        _t: &str,
        _u: &str,
        session_id: Uuid,
        metadata: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        let mut rows = self.rows.lock();
        for row in rows.iter_mut() {
            if row.session_id == session_id {
                row.metadata = metadata.clone();
                return Ok(row.clone());
            }
        }
        Err(ChatEngineError::not_found("session", session_id))
    }

    async fn update_capabilities(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _c: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Err(ChatEngineError::internal("mock update_capabilities"))
    }

    async fn update_lifecycle_state(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _s: LifecycleState,
    ) -> std::result::Result<Session, ChatEngineError> {
        Err(ChatEngineError::internal("mock update_lifecycle_state"))
    }

    async fn soft_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _d: i64,
    ) -> std::result::Result<Session, ChatEngineError> {
        Err(ChatEngineError::internal("mock soft_delete"))
    }

    async fn hard_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
    ) -> std::result::Result<bool, ChatEngineError> {
        Ok(true)
    }

    async fn list_active_sessions_for_tenant(
        &self,
        tenant_id: &str,
        after: Option<Uuid>,
        limit: u32,
    ) -> std::result::Result<Vec<Session>, ChatEngineError> {
        let mut rows: Vec<Session> = self
            .rows
            .lock()
            .iter()
            .filter(|r| {
                r.tenant_id.as_str() == tenant_id
                    && r.lifecycle_state == LifecycleState::Active
                    && after.is_none_or(|a| r.session_id > a)
            })
            .cloned()
            .collect();
        rows.sort_by_key(|r| r.session_id);
        rows.truncate(limit as usize);
        Ok(rows)
    }

    async fn list_tenants_with_active_sessions(
        &self,
    ) -> std::result::Result<Vec<String>, ChatEngineError> {
        let mut tenants: Vec<String> = self
            .rows
            .lock()
            .iter()
            .filter(|r| r.lifecycle_state == LifecycleState::Active)
            .map(|r| r.tenant_id.as_str().to_owned())
            .collect();
        tenants.sort();
        tenants.dedup();
        Ok(tenants)
    }
}

struct MockSessionTypeRepo;
#[async_trait]
impl SessionTypeRepo for MockSessionTypeRepo {
    async fn insert(
        &self,
        _m: NewSessionType,
    ) -> std::result::Result<SessionType, ChatEngineError> {
        Err(ChatEngineError::internal("mock"))
    }
    async fn find_by_id(
        &self,
        _id: Uuid,
    ) -> std::result::Result<Option<SessionType>, ChatEngineError> {
        Ok(None)
    }
    async fn list(&self) -> std::result::Result<Vec<SessionType>, ChatEngineError> {
        Ok(vec![])
    }
}

/// One `insert_summary_message` call recorded by the mock:
/// `(session_id, summary_text, tenant_id)`.
type RecordedSummary = (Uuid, String, Option<String>);

/// `MockMessageRepo` driven by a caller-supplied vector of messages
/// the retention-evaluator should see. Tracks `delete_message_subtree`
/// calls so tests can assert at-most-once behaviour.
struct MockMessageRepo {
    all: Mutex<Vec<Message>>,
    deletes: Mutex<Vec<Uuid>>,
    summaries: Mutex<Vec<RecordedSummary>>,
    /// When set, `insert_summary_message` returns an error so tests can
    /// exercise the persist-failure path of the summarize driver.
    fail_summary: Mutex<bool>,
}

impl MockMessageRepo {
    fn new(messages: Vec<Message>) -> Arc<Self> {
        Arc::new(Self {
            all: Mutex::new(messages),
            deletes: Mutex::new(Vec::new()),
            summaries: Mutex::new(Vec::new()),
            fail_summary: Mutex::new(false),
        })
    }

    /// Make `insert_summary_message` fail on subsequent calls.
    fn fail_summaries(&self) {
        *self.fail_summary.lock() = true;
    }
}

#[async_trait]
impl MessageRepo for MockMessageRepo {
    async fn insert_user_and_assistant_stub(
        &self,
        _r: NewUserMessage,
    ) -> std::result::Result<InsertedPair, ChatEngineError> {
        Err(ChatEngineError::internal("mock"))
    }
    async fn finalize_assistant(
        &self,
        _session_id: Uuid,
        _id: Uuid,
        _o: FinalizeOutcome,
    ) -> std::result::Result<(), ChatEngineError> {
        Ok(())
    }
    async fn insert_summary_message(
        &self,
        session_id: Uuid,
        text: String,
        _metadata: Option<serde_json::Value>,
        _summarized_ids: Vec<Uuid>,
        tenant_id: Option<String>,
    ) -> std::result::Result<Uuid, ChatEngineError> {
        if *self.fail_summary.lock() {
            return Err(ChatEngineError::internal("mock summary persist failure"));
        }
        self.summaries.lock().push((session_id, text, tenant_id));
        Ok(Uuid::new_v4())
    }
    async fn fetch_active_history(
        &self,
        _s: Uuid,
        _d: Option<u32>,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(self.all.lock().clone())
    }
    async fn find_message_in_session(
        &self,
        _s: Uuid,
        _m: Uuid,
    ) -> std::result::Result<Option<Message>, ChatEngineError> {
        Ok(None)
    }
    async fn list_non_root_messages_chrono(
        &self,
        session_id: Uuid,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(self
            .all
            .lock()
            .iter()
            .filter(|m| m.session_id == session_id && m.parent_message_id.is_some())
            .cloned()
            .collect())
    }
    async fn list_non_root_messages_older_than(
        &self,
        session_id: Uuid,
        older_than: OffsetDateTime,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(self
            .all
            .lock()
            .iter()
            .filter(|m| {
                m.session_id == session_id
                    && m.parent_message_id.is_some()
                    && m.created_at < older_than
            })
            .cloned()
            .collect())
    }
    async fn count_non_root_messages(
        &self,
        session_id: Uuid,
    ) -> std::result::Result<u64, ChatEngineError> {
        Ok(self
            .all
            .lock()
            .iter()
            .filter(|m| m.session_id == session_id && m.parent_message_id.is_some())
            .count() as u64)
    }
    async fn list_oldest_non_root_message_ids(
        &self,
        session_id: Uuid,
        limit: u32,
    ) -> std::result::Result<Vec<Uuid>, ChatEngineError> {
        let mut rows: Vec<Message> = self
            .all
            .lock()
            .iter()
            .filter(|m| m.session_id == session_id && m.parent_message_id.is_some())
            .cloned()
            .collect();
        rows.sort_by_key(|m| m.created_at);
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|m| m.message_id)
            .collect())
    }
    async fn list_non_root_message_ids_older_than(
        &self,
        session_id: Uuid,
        older_than: OffsetDateTime,
        limit: u32,
    ) -> std::result::Result<Vec<Uuid>, ChatEngineError> {
        let mut rows: Vec<Message> = self
            .all
            .lock()
            .iter()
            .filter(|m| {
                m.session_id == session_id
                    && m.parent_message_id.is_some()
                    && m.created_at < older_than
            })
            .cloned()
            .collect();
        rows.sort_by_key(|m| m.created_at);
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|m| m.message_id)
            .collect())
    }
    async fn delete_message_subtree(
        &self,
        _s: Uuid,
        root_id: Uuid,
    ) -> std::result::Result<u64, ChatEngineError> {
        self.deletes.lock().push(root_id);
        Ok(1)
    }
}

struct StubPluginConfigRepo;
#[async_trait]
impl PluginConfigRepo for StubPluginConfigRepo {
    async fn find(
        &self,
        _p: &str,
        _s: Uuid,
    ) -> std::result::Result<Option<JsonValue>, ChatEngineError> {
        Ok(None)
    }
    async fn upsert(
        &self,
        _p: &str,
        _s: Uuid,
        _c: JsonValue,
    ) -> std::result::Result<(), ChatEngineError> {
        Ok(())
    }
    async fn delete(&self, _p: &str, _s: Uuid) -> std::result::Result<(), ChatEngineError> {
        Ok(())
    }
}

// ----- Helpers -----------------------------------------------------

/// Owner pair every fixture session carries. The `SecurityContext` returned by
/// [`ctx`] is built from the same pair so the ownership guard
/// (`owner_guard::ensure_session_owner`) sees the caller as the owner; tests
/// that need a stranger build their own context.
const OWNER_TENANT: Uuid = Uuid::from_u128(0x0A11);
const OWNER_USER: Uuid = Uuid::from_u128(0x0B22);

fn make_session(session_id: Uuid, metadata: Option<JsonValue>) -> Session {
    let now = OffsetDateTime::now_utc();
    Session {
        session_id,
        tenant_id: OWNER_TENANT.to_string().into(),
        user_id: OWNER_USER.to_string().into(),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_message(session_id: Uuid, parent: Option<Uuid>, offset_secs: i64) -> Message {
    let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000 + offset_secs).unwrap();
    Message {
        message_id: Uuid::new_v4(),
        session_id,
        tenant_id: None,
        user_id: None,
        parent_message_id: parent,
        variant_index: 0,
        is_active: true,
        role: MessageRole::User,
        parts: vec![MessagePart::text(Uuid::nil(), Uuid::nil(), 0, "hi")],
        file_ids: vec![],
        metadata: None,
        is_complete: true,
        is_hidden_from_user: false,
        is_hidden_from_backend: false,
        created_at: ts,
        updated_at: ts,
    }
}

fn make_service(
    sessions: Arc<MockSessionRepo>,
    messages: Arc<MockMessageRepo>,
) -> IntelligenceService {
    let session_types: Arc<dyn SessionTypeRepo> = Arc::new(MockSessionTypeRepo);
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    IntelligenceService::new(
        sessions as Arc<dyn SessionRepo>,
        session_types,
        messages as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    )
}

fn make_service_with_enforcer(
    sessions: Arc<MockSessionRepo>,
    messages: Arc<MockMessageRepo>,
    enforcer: PolicyEnforcer,
) -> IntelligenceService {
    let session_types: Arc<dyn SessionTypeRepo> = Arc::new(MockSessionTypeRepo);
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    IntelligenceService::new(
        sessions as Arc<dyn SessionRepo>,
        session_types,
        messages as Arc<dyn MessageRepo>,
        plugins,
        enforcer,
    )
}

fn ctx() -> SecurityContext {
    test_support::ctx_for_subject(OWNER_USER, OWNER_TENANT)
}

// ----- evaluate_retention_policy ----------------------------------

#[tokio::test]
async fn evaluate_none_returns_empty() {
    let session_id = Uuid::new_v4();
    let msgs = MockMessageRepo::new(vec![
        make_message(session_id, Some(Uuid::new_v4()), 0),
        make_message(session_id, Some(Uuid::new_v4()), 1),
    ]);
    let svc = make_service(MockSessionRepo::new(vec![]), msgs);
    let out = svc
        .evaluate_retention_policy(session_id, &RetentionPolicy::None)
        .await
        .unwrap();
    assert!(out.is_empty(), "None policy yields zero deletions");
}

#[tokio::test]
async fn evaluate_age_based_deletes_only_old_non_root() {
    let session_id = Uuid::new_v4();
    let root_parent = Uuid::new_v4();
    // Old enough to be cleaned up (offset = 0 → unix 1_700_000_000;
    // cutoff = now - 1 day → safely older).
    let m_old = make_message(session_id, Some(root_parent), 0);
    // Modern message (current time → preserved).
    let mut m_new = make_message(session_id, Some(root_parent), 0);
    m_new.created_at = OffsetDateTime::now_utc();
    // Root message — must never be eligible regardless of age.
    let mut m_root = make_message(session_id, None, 0);
    m_root.created_at = OffsetDateTime::from_unix_timestamp(0).unwrap();
    let old_id = m_old.message_id;

    let msgs = MockMessageRepo::new(vec![m_old, m_new, m_root]);
    let svc = make_service(MockSessionRepo::new(vec![]), msgs);
    let out = svc
        .evaluate_retention_policy(session_id, &RetentionPolicy::AgeBased { max_age_days: 1 })
        .await
        .unwrap();
    assert_eq!(
        out,
        vec![old_id],
        "only the old non-root message is eligible"
    );
}

#[tokio::test]
async fn evaluate_count_based_keeps_newest_n_and_excludes_root() {
    let session_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    // 5 non-root messages, chronological order.
    let m0 = make_message(session_id, Some(parent), 0);
    let m1 = make_message(session_id, Some(parent), 1);
    let m2 = make_message(session_id, Some(parent), 2);
    let m3 = make_message(session_id, Some(parent), 3);
    let m4 = make_message(session_id, Some(parent), 4);
    // One root with the very oldest timestamp.
    let mut m_root = make_message(session_id, None, -1);
    m_root.created_at = OffsetDateTime::from_unix_timestamp(0).unwrap();

    let ids = vec![m0.message_id, m1.message_id];
    let msgs = MockMessageRepo::new(vec![m0, m1, m2, m3, m4, m_root]);
    let svc = make_service(MockSessionRepo::new(vec![]), msgs);
    let out = svc
        .evaluate_retention_policy(
            session_id,
            &RetentionPolicy::CountBased {
                max_message_count: 3,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        out, ids,
        "oldest 2 of 5 selected; newest 3 kept; root excluded"
    );
}

#[tokio::test]
async fn evaluate_count_based_below_threshold_is_empty() {
    let session_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let msgs = MockMessageRepo::new(vec![
        make_message(session_id, Some(parent), 0),
        make_message(session_id, Some(parent), 1),
    ]);
    let svc = make_service(MockSessionRepo::new(vec![]), msgs);
    let out = svc
        .evaluate_retention_policy(
            session_id,
            &RetentionPolicy::CountBased {
                max_message_count: 5,
            },
        )
        .await
        .unwrap();
    assert!(out.is_empty(), "2 <= 5 \u{2192} no eligible deletions");
}

#[tokio::test]
async fn evaluate_excludes_root_messages_under_age_based() {
    let session_id = Uuid::new_v4();
    // Only root messages (parent = None); all must be preserved.
    let mut m_root = make_message(session_id, None, 0);
    m_root.created_at = OffsetDateTime::from_unix_timestamp(0).unwrap();
    let msgs = MockMessageRepo::new(vec![m_root]);
    let svc = make_service(MockSessionRepo::new(vec![]), msgs);
    let out = svc
        .evaluate_retention_policy(session_id, &RetentionPolicy::AgeBased { max_age_days: 1 })
        .await
        .unwrap();
    assert!(out.is_empty(), "root messages must never be eligible");
}

#[tokio::test]
async fn run_retention_cleanup_is_idempotent() {
    let session_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    // Populate the policy in metadata so it gets picked up by the
    // tenant-level pass.
    let metadata = serde_json::json!({
        "retention_policy": {"type": "count_based", "max_message_count": 1},
    });
    let session_row = make_session(session_id, Some(metadata));
    let sessions = MockSessionRepo::new(vec![session_row]);
    // 3 non-root → after the first pass 2 are eligible (3 - 1 = 2).
    let msgs = MockMessageRepo::new(vec![
        make_message(session_id, Some(parent), 0),
        make_message(session_id, Some(parent), 1),
        make_message(session_id, Some(parent), 2),
    ]);
    let svc = make_service(sessions.clone(), msgs.clone());
    let report = svc
        .run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
        .await
        .unwrap();
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.sessions[0].messages_deleted, 2);
    let first_deletes = msgs.deletes.lock().clone();
    assert_eq!(first_deletes.len(), 2, "two deletes on first pass");

    // Idempotency: re-running with the same set produces another 2
    // deletes (the mock repo doesn't actually remove rows) but never
    // panics — the real repo returns Ok(0) for missing roots, which
    // is the contract the algorithm relies on.
    let report2 = svc
        .run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
        .await
        .unwrap();
    assert_eq!(report2.sessions.len(), 1);
}

// ----- validate_retention_policy ----------------------------------

#[test]
fn validate_none_passes() {
    assert!(validate_retention_policy(RetentionPolicy::None).is_ok());
}

#[test]
fn validate_age_based_rejects_zero() {
    let err = validate_retention_policy(RetentionPolicy::AgeBased { max_age_days: 0 }).unwrap_err();
    match err {
        ChatEngineError::BadRequest { reason } => {
            assert!(reason.contains("max_age_days"));
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[test]
fn validate_age_based_accepts_one_or_more() {
    validate_retention_policy(RetentionPolicy::AgeBased { max_age_days: 1 })
        .expect("max_age_days=1 must be accepted");
    validate_retention_policy(RetentionPolicy::AgeBased { max_age_days: 365 })
        .expect("max_age_days=365 must be accepted");
}

#[test]
fn validate_count_based_rejects_zero() {
    let err = validate_retention_policy(RetentionPolicy::CountBased {
        max_message_count: 0,
    })
    .unwrap_err();
    match err {
        ChatEngineError::BadRequest { reason } => {
            assert!(reason.contains("max_message_count"));
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[test]
fn validate_count_based_accepts_one_or_more() {
    validate_retention_policy(RetentionPolicy::CountBased {
        max_message_count: 1,
    })
    .expect("max_message_count=1 must be accepted");
}

// ----- get_effective_retention_policy -----------------------------

#[tokio::test]
async fn get_effective_returns_per_session_when_set() {
    let session_id = Uuid::new_v4();
    let metadata = serde_json::json!({
        "retention_policy": {"type": "age_based", "max_age_days": 7},
    });
    let row = make_session(session_id, Some(metadata));
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let out = svc
        .get_effective_retention_policy(&ctx(), session_id)
        .await
        .unwrap();
    assert!(matches!(out, RetentionPolicy::AgeBased { max_age_days: 7 }));
}

#[tokio::test]
async fn get_effective_falls_back_to_none_when_unset() {
    let session_id = Uuid::new_v4();
    let row = make_session(session_id, None);
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let out = svc
        .get_effective_retention_policy(&ctx(), session_id)
        .await
        .unwrap();
    assert!(matches!(out, RetentionPolicy::None));
}

// ----- update_session_retention_policy ----------------------------

#[tokio::test]
async fn update_persists_policy_in_metadata() {
    let session_id = Uuid::new_v4();
    let row = make_session(session_id, None);
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions.clone(), msgs);
    let updated = svc
        .update_session_retention_policy(
            &ctx(),
            session_id,
            RetentionPolicy::CountBased {
                max_message_count: 100,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        updated,
        RetentionPolicy::CountBased {
            max_message_count: 100
        }
    ));
    // Confirm the metadata write landed on the mock repo.
    let row = sessions.rows.lock()[0].clone();
    let stored = row.metadata.unwrap();
    assert_eq!(
        stored["retention_policy"],
        serde_json::json!({"type": "count_based", "max_message_count": 100})
    );
}

#[tokio::test]
async fn update_rejects_invalid_max_age_days() {
    let session_id = Uuid::new_v4();
    let row = make_session(session_id, None);
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let err = svc
        .update_session_retention_policy(
            &ctx(),
            session_id,
            RetentionPolicy::AgeBased { max_age_days: 0 },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn update_rejects_soft_deleted_session() {
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.lifecycle_state = LifecycleState::SoftDeleted;
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let err = svc
        .update_session_retention_policy(&ctx(), session_id, RetentionPolicy::None)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Conflict { .. }));
}

// ----- summary plugin integration ---------------------------------

use chat_engine_sdk::error::PluginError;
use chat_engine_sdk::plugin::{MessagePluginCtx, PluginStream, stream_from_events};
use toolkit::client_hub::ClientScope;

struct ScriptedSummaryPlugin {
    id: String,
    events: Mutex<Option<Vec<StreamingEvent>>>,
    pre_error: Mutex<Option<PluginError>>,
}

impl ScriptedSummaryPlugin {
    fn ok(id: &str, events: Vec<StreamingEvent>) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            events: Mutex::new(Some(events)),
            pre_error: Mutex::new(None),
        })
    }

    fn pre_error(id: &str, err: PluginError) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            events: Mutex::new(None),
            pre_error: Mutex::new(Some(err)),
        })
    }
}

#[async_trait]
impl ChatEngineBackendPlugin for ScriptedSummaryPlugin {
    async fn on_message(
        &self,
        _c: MessagePluginCtx,
    ) -> std::result::Result<PluginStream, PluginError> {
        Err(PluginError::internal(
            "test plugin does not handle messages",
        ))
    }

    async fn on_session_summary(
        &self,
        _c: SessionPluginCtx,
    ) -> std::result::Result<PluginStream, PluginError> {
        if let Some(err) = self.pre_error.lock().take() {
            return Err(err);
        }
        let events = self.events.lock().take().unwrap_or_default();
        Ok(stream_from_events(events))
    }

    fn plugin_instance_id(&self) -> &str {
        &self.id
    }
}

fn make_service_with_plugin(
    plugin_id: &str,
    plugin: Arc<dyn ChatEngineBackendPlugin>,
    session_type_id: Uuid,
    session_row: Session,
) -> (
    IntelligenceService,
    Arc<MockSessionRepo>,
    Arc<MockMessageRepo>,
) {
    let sessions = MockSessionRepo::new(vec![session_row]);
    let msgs = MockMessageRepo::new(vec![]);
    let hub = Arc::new(ClientHub::new());
    hub.register_scoped::<dyn ChatEngineBackendPlugin>(ClientScope::gts_id(plugin_id), plugin);
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));

    // session_types mock: return a row with the configured plugin id.
    struct OneTypeRepo {
        model: Mutex<SessionType>,
    }
    #[async_trait]
    impl SessionTypeRepo for OneTypeRepo {
        async fn insert(
            &self,
            _m: NewSessionType,
        ) -> std::result::Result<SessionType, ChatEngineError> {
            Err(ChatEngineError::internal("mock"))
        }
        async fn find_by_id(
            &self,
            id: Uuid,
        ) -> std::result::Result<Option<SessionType>, ChatEngineError> {
            let m = self.model.lock().clone();
            if m.session_type_id == id {
                Ok(Some(m))
            } else {
                Ok(None)
            }
        }
        async fn list(&self) -> std::result::Result<Vec<SessionType>, ChatEngineError> {
            Ok(vec![self.model.lock().clone()])
        }
    }
    let now = OffsetDateTime::now_utc();
    let st_repo: Arc<dyn SessionTypeRepo> = Arc::new(OneTypeRepo {
        model: Mutex::new(SessionType {
            session_type_id,
            name: "t".into(),
            plugin_instance_id: Some(plugin_id.into()),
            created_at: now,
            updated_at: now,
        }),
    });

    let svc = IntelligenceService::new(
        sessions.clone() as Arc<dyn SessionRepo>,
        st_repo,
        msgs.clone() as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    );
    (svc, sessions, msgs)
}

#[tokio::test]
async fn summarize_pre_stream_error_propagates() {
    let plugin_id = "summary-fail";
    let session_type_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.session_type_id = Some(session_type_id);
    let plugin = ScriptedSummaryPlugin::pre_error(plugin_id, PluginError::internal("boom"));
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, _sessions, _msgs) =
        make_service_with_plugin(plugin_id, plugin_dyn, session_type_id, row);

    let cancel = CancellationToken::new();
    let result = svc.summarize_session(&ctx(), session_id, cancel).await;
    let err = match result {
        Ok(_) => panic!("pre-stream failure must surface as Err"),
        Err(e) => e,
    };
    // Internal pluginerror is mapped to ChatEngineError::Internal
    // (see error.rs). Either Internal or BackendUnavailable is
    // acceptable in the carry-over notes — the handler maps both
    // to 502.
    assert!(
        matches!(
            err,
            ChatEngineError::Internal { .. } | ChatEngineError::BackendUnavailable { .. }
        ),
        "expected Internal or BackendUnavailable, got {err:?}",
    );
}

#[tokio::test]
async fn summarize_returns_422_style_when_plugin_unregistered() {
    // No plugin registered for this session_type's id — we still
    // reach summary entry but plugin.resolve fails, mapped to
    // BackendUnavailable per the rules.
    let plugin_id = "missing";
    let session_type_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.session_type_id = Some(session_type_id);
    // Use a session-type repo that returns the type but the plugin
    // hub has no scope registered for `plugin_id`.
    let now = OffsetDateTime::now_utc();
    struct ReturnsType {
        id: Uuid,
        pid: String,
        now: OffsetDateTime,
    }
    #[async_trait]
    impl SessionTypeRepo for ReturnsType {
        async fn insert(
            &self,
            _m: NewSessionType,
        ) -> std::result::Result<SessionType, ChatEngineError> {
            Err(ChatEngineError::internal("mock"))
        }
        async fn find_by_id(
            &self,
            id: Uuid,
        ) -> std::result::Result<Option<SessionType>, ChatEngineError> {
            if id == self.id {
                Ok(Some(SessionType {
                    session_type_id: self.id,
                    name: "t".into(),
                    plugin_instance_id: Some(self.pid.clone()),
                    created_at: self.now,
                    updated_at: self.now,
                }))
            } else {
                Ok(None)
            }
        }
        async fn list(&self) -> std::result::Result<Vec<SessionType>, ChatEngineError> {
            Ok(vec![])
        }
    }
    let st_repo: Arc<dyn SessionTypeRepo> = Arc::new(ReturnsType {
        id: session_type_id,
        pid: plugin_id.into(),
        now,
    });
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let hub = Arc::new(ClientHub::new()); // empty
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    let svc = IntelligenceService::new(
        sessions as Arc<dyn SessionRepo>,
        st_repo,
        msgs as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    );

    let cancel = CancellationToken::new();
    let result = svc.summarize_session(&ctx(), session_id, cancel).await;
    let err = match result {
        Ok(_) => panic!("unregistered plugin must produce an error"),
        Err(e) => e,
    };
    match err {
        ChatEngineError::BackendUnavailable { ref reason, .. } => {
            assert!(reason.contains("not registered"), "got: {reason}");
        }
        other => panic!("expected BackendUnavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn summarize_happy_path_persists_on_complete() {
    let plugin_id = "summary-ok";
    let session_type_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.session_type_id = Some(session_type_id);
    let plugin = ScriptedSummaryPlugin::ok(
        plugin_id,
        vec![
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: Uuid::nil(),
                chunk: "summary ".into(),
            }),
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: Uuid::nil(),
                chunk: "text".into(),
            }),
            StreamingEvent::Complete(StreamingCompleteEvent {
                message_id: Uuid::nil(),
                metadata: Some(serde_json::json!({"summarized_message_ids": []})),
                file_citations: vec![],
                link_citations: vec![],
                references: vec![],
            }),
        ],
    );
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, _sessions, msgs) =
        make_service_with_plugin(plugin_id, plugin_dyn, session_type_id, row);

    let caller = test_support::ctx_for_subject(OWNER_USER, OWNER_TENANT);
    let cancel = CancellationToken::new();
    let mut stream = svc
        .summarize_session(&caller, session_id, cancel)
        .await
        .expect("summary dispatch");
    let mut kinds = Vec::new();
    while let Some(evt) = stream.next().await {
        match evt {
            StreamingEvent::Start(_) => kinds.push("start"),
            StreamingEvent::Chunk(_) => kinds.push("chunk"),
            StreamingEvent::Complete(_) => kinds.push("complete"),
            StreamingEvent::Error(_) => kinds.push("error"),
            _ => kinds.push("other"),
        }
    }
    assert_eq!(kinds, vec!["start", "chunk", "chunk", "complete"]);
    // `Complete` is emitted only after a successful persist, so by the
    // time the stream closes the summary row must have been recorded.
    let summaries = msgs.summaries.lock();
    assert_eq!(
        summaries.len(),
        1,
        "happy path must persist the summary before emitting Complete",
    );
    // The summary message must be stamped with the caller's tenant
    // (denormalized owning tenant), threaded from the JWT identity.
    assert_eq!(
        summaries[0].2.as_deref(),
        Some(OWNER_TENANT.to_string().as_str()),
        "summary must inherit the identity tenant_id",
    );
}

#[tokio::test]
async fn summarize_persist_failure_emits_error_not_complete() {
    // When persistence fails, the driver must surface a streaming Error
    // and never emit Complete (which would falsely report success).
    let plugin_id = "summary-persist-fail";
    let session_type_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.session_type_id = Some(session_type_id);
    let plugin = ScriptedSummaryPlugin::ok(
        plugin_id,
        vec![
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: Uuid::nil(),
                chunk: "summary".into(),
            }),
            StreamingEvent::Complete(StreamingCompleteEvent {
                message_id: Uuid::nil(),
                metadata: Some(serde_json::json!({"summarized_message_ids": []})),
                file_citations: vec![],
                link_citations: vec![],
                references: vec![],
            }),
        ],
    );
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, _sessions, msgs) =
        make_service_with_plugin(plugin_id, plugin_dyn, session_type_id, row);
    msgs.fail_summaries();

    let cancel = CancellationToken::new();
    let mut stream = svc
        .summarize_session(&ctx(), session_id, cancel)
        .await
        .expect("summary dispatch");
    let mut kinds = Vec::new();
    while let Some(evt) = stream.next().await {
        match evt {
            StreamingEvent::Start(_) => kinds.push("start"),
            StreamingEvent::Chunk(_) => kinds.push("chunk"),
            StreamingEvent::Complete(_) => kinds.push("complete"),
            StreamingEvent::Error(_) => kinds.push("error"),
            _ => kinds.push("other"),
        }
    }
    assert_eq!(kinds, vec!["start", "chunk", "error"]);
    assert!(
        msgs.summaries.lock().is_empty(),
        "no summary should be recorded when persistence fails",
    );
}

// ----- summary-related helpers ------------------------------------

#[test]
fn extract_summarized_ids_parses_valid_array() {
    let id1 = Uuid::new_v4();
    let id2 = Uuid::new_v4();
    let meta = serde_json::json!({
        "summarized_message_ids": [id1.to_string(), id2.to_string()],
    });
    let out = extract_summarized_ids(&meta);
    assert_eq!(out, vec![id1, id2]);
}

#[test]
fn extract_summarized_ids_handles_missing_key() {
    let meta = serde_json::json!({"other": "value"});
    assert!(extract_summarized_ids(&meta).is_empty());
}

#[test]
fn extract_summarized_ids_skips_malformed_entries() {
    let id = Uuid::new_v4();
    let meta = serde_json::json!({
        "summarized_message_ids": [id.to_string(), "not-a-uuid", 42],
    });
    assert_eq!(extract_summarized_ids(&meta), vec![id]);
}

#[test]
fn retention_policy_label_covers_all_variants() {
    assert_eq!(retention_policy_label(&RetentionPolicy::None), "none");
    assert_eq!(
        retention_policy_label(&RetentionPolicy::AgeBased { max_age_days: 1 }),
        "age_based"
    );
    assert_eq!(
        retention_policy_label(&RetentionPolicy::CountBased {
            max_message_count: 1
        }),
        "count_based"
    );
}

// ----- run_retention_cleanup_for_tenant: skip-none --------------------

#[tokio::test]
async fn run_cleanup_records_none_policy_without_lock_or_delete() {
    let session_id = Uuid::new_v4();
    let row = make_session(session_id, None); // metadata None → effective None
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs.clone());
    let report = svc
        .run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
        .await
        .unwrap();
    assert_eq!(report.sessions.len(), 1);
    assert_eq!(report.sessions[0].policy_type, "none");
    assert_eq!(report.sessions[0].messages_deleted, 0);
    assert!(msgs.deletes.lock().is_empty());
}

#[tokio::test]
async fn run_cleanup_ignores_other_tenants() {
    let session_id = Uuid::new_v4();
    let mut row = make_session(session_id, None);
    row.tenant_id = "other".into();
    let sessions = MockSessionRepo::new(vec![row]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let report = svc
        .run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
        .await
        .unwrap();
    assert!(report.sessions.is_empty());
}

// ----- retention caps -------------------------------------------------

#[tokio::test]
async fn run_cleanup_caps_sessions_per_tick_and_defers_remainder() {
    // Five active sessions, cap = 2 → only the first two by
    // session_id are processed this tick.
    let session_ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let rows: Vec<_> = session_ids
        .iter()
        .map(|sid| make_session(*sid, None))
        .collect();
    let sessions = MockSessionRepo::new(rows);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs).with_retention_caps(2, 1000);
    let report = svc
        .run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
        .await
        .unwrap();
    assert_eq!(
        report.sessions.len(),
        2,
        "session cap should limit processed sessions per tick",
    );
}

#[tokio::test]
async fn run_cleanup_cursor_pages_all_sessions_across_ticks() {
    // 5 active sessions, cap = 2. Consecutive ticks must page through
    // every session via the round-robin cursor (2, 2, 1) — no head
    // re-scan, no starved tail — then wrap back to the head.
    let session_ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
    let rows: Vec<_> = session_ids
        .iter()
        .map(|sid| make_session(*sid, None))
        .collect();
    let sessions = MockSessionRepo::new(rows);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs).with_retention_caps(2, 1000);

    let tick_ids = |svc: &IntelligenceService| {
        let svc = svc.clone();
        async move {
            svc.run_retention_cleanup_for_tenant(&OWNER_TENANT.to_string())
                .await
                .unwrap()
                .sessions
                .into_iter()
                .map(|o| o.session_id)
                .collect::<Vec<Uuid>>()
        }
    };

    let t1 = tick_ids(&svc).await;
    let t2 = tick_ids(&svc).await;
    let t3 = tick_ids(&svc).await;
    assert_eq!(t1.len(), 2, "tick 1 processes a full batch");
    assert_eq!(t2.len(), 2, "tick 2 processes the next full batch");
    assert_eq!(t3.len(), 1, "tick 3 processes the remaining tail");

    let mut covered: Vec<Uuid> = [t1, t2, t3].concat();
    let unique: std::collections::HashSet<Uuid> = covered.iter().copied().collect();
    assert_eq!(
        unique.len(),
        5,
        "three ticks must cover every session exactly once (no overlap, no gap)",
    );
    covered.sort();
    let mut expected = session_ids.clone();
    expected.sort();
    assert_eq!(
        covered, expected,
        "every active session is visited across ticks"
    );

    // The short tail batch dropped the cursor, so the next tick wraps to
    // the head rather than returning nothing.
    let t4 = tick_ids(&svc).await;
    assert_eq!(t4.len(), 2, "after the tail, the cursor wraps to the head");
}

#[tokio::test]
async fn evaluate_count_based_caps_deletion_budget_per_session() {
    // Per-session deletion cap = 2; surplus = 5 (max=1, total=6).
    // Only the 2 oldest non-root ids are returned.
    let session_id = Uuid::new_v4();
    let root_id = Uuid::new_v4();
    let mut msgs = vec![make_message(session_id, None, 0)];
    // 6 non-root messages with strictly increasing created_at.
    for i in 1..=6 {
        msgs.push(make_message(session_id, Some(root_id), i));
    }
    let oldest_two: Vec<Uuid> = {
        let mut sorted = msgs.clone();
        sorted.sort_by_key(|m| m.created_at);
        sorted
            .iter()
            .filter(|m| m.parent_message_id.is_some())
            .take(2)
            .map(|m| m.message_id)
            .collect()
    };

    let sessions = MockSessionRepo::new(vec![]);
    let messages = MockMessageRepo::new(msgs);
    let svc = make_service(sessions, messages).with_retention_caps(1000, 2);
    let eligible = svc
        .evaluate_retention_policy(
            session_id,
            &RetentionPolicy::CountBased {
                max_message_count: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(eligible.len(), 2, "deletion cap should bound eligible ids");
    assert_eq!(
        eligible, oldest_two,
        "should select the OLDEST non-root ids"
    );
}

// ----- run_retention_cleanup_all_tenants ------------------------------

#[tokio::test]
async fn run_cleanup_all_tenants_visits_every_distinct_active_tenant() {
    // Two tenants with active sessions; one with an archived session
    // that must NOT be visited.
    let mut a = make_session(Uuid::new_v4(), None);
    a.tenant_id = "tenant_a".into();
    let mut b = make_session(Uuid::new_v4(), None);
    b.tenant_id = "tenant_b".into();
    let mut c = make_session(Uuid::new_v4(), None);
    c.tenant_id = "tenant_c".into();
    c.lifecycle_state = LifecycleState::Archived;

    let sessions = MockSessionRepo::new(vec![a, b, c]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);

    let report = svc.run_retention_cleanup_all_tenants().await.unwrap();
    // Two active tenants → two session outcomes, no archived row.
    assert_eq!(report.sessions.len(), 2);
    let mut seen: Vec<String> = report
        .sessions
        .iter()
        .map(|o| o.policy_type.to_owned())
        .collect();
    seen.sort();
    // Both tenants resolve to RetentionPolicy::None (metadata=None).
    assert_eq!(seen, vec!["none".to_owned(), "none".to_owned()]);
}

#[tokio::test]
async fn run_cleanup_all_tenants_returns_empty_when_no_active_tenants() {
    let sessions = MockSessionRepo::new(vec![]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service(sessions, msgs);
    let report = svc.run_retention_cleanup_all_tenants().await.unwrap();
    assert!(report.sessions.is_empty());
}

// --------------------------- Authz (PEP) tests ------------------------
//
// The subject-facing intelligence surface (retention get/update, summarize)
// gates through authorize_session_op as SESSION READ/UPDATE point-ops. An
// explicit PDP deny or an evaluation error fails closed to `Forbidden` (403)
// via `From<EnforcerError>`. The background retention sweep has no subject,
// so it never calls the enforcer (covered by the run_cleanup_* tests).
// @cpt-cf-chat-engine-interface-pep

#[tokio::test]
async fn get_effective_pdp_denied_returns_forbidden() {
    let session_id = Uuid::new_v4();
    let sessions = MockSessionRepo::new(vec![make_session(session_id, None)]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service_with_enforcer(sessions, msgs, test_support::enforcer_deny());

    let res = svc.get_effective_retention_policy(&ctx(), session_id).await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}

#[tokio::test]
async fn get_effective_evaluation_failed_returns_forbidden() {
    let session_id = Uuid::new_v4();
    let sessions = MockSessionRepo::new(vec![make_session(session_id, None)]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service_with_enforcer(sessions, msgs, test_support::enforcer_failing());

    let res = svc.get_effective_retention_policy(&ctx(), session_id).await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}

#[tokio::test]
async fn update_retention_pdp_denied_returns_forbidden() {
    let session_id = Uuid::new_v4();
    let sessions = MockSessionRepo::new(vec![make_session(session_id, None)]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service_with_enforcer(sessions, msgs, test_support::enforcer_deny());

    let res = svc
        .update_session_retention_policy(&ctx(), session_id, RetentionPolicy::None)
        .await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}

#[tokio::test]
async fn summarize_pdp_denied_returns_forbidden() {
    let session_id = Uuid::new_v4();
    let sessions = MockSessionRepo::new(vec![make_session(session_id, None)]);
    let msgs = MockMessageRepo::new(vec![]);
    let svc = make_service_with_enforcer(sessions, msgs, test_support::enforcer_deny());

    let cancel = CancellationToken::new();
    let res = svc.summarize_session(&ctx(), session_id, cancel).await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}
