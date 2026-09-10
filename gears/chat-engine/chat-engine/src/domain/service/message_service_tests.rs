use super::*;
use crate::domain::ports::NewSession;
use crate::domain::ports::NewSessionType;
use crate::domain::service::test_support;
use crate::domain::session::LifecycleState;
use crate::domain::session::Session;
use crate::domain::session::SessionType;
use uuid::Uuid;

use async_trait::async_trait;
use chat_engine_sdk::plugin::ChatEngineBackendPlugin;
use chat_engine_sdk::plugin::stream_from_events;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use time::OffsetDateTime;
use toolkit::ClientHub;
use toolkit::client_hub::ClientScope;

use crate::domain::ports::InsertedPair;
use crate::domain::ports::PluginConfigRepo;

// ----------------- Mocks -----------------

struct MockSessionRepo {
    session: Mutex<Session>,
}

impl MockSessionRepo {
    fn new(session_type_id: Option<Uuid>, capabilities: Option<JsonValue>) -> Arc<Self> {
        let now = OffsetDateTime::now_utc();
        Arc::new(Self {
            session: Mutex::new(Session {
                session_id: Uuid::new_v4(),
                tenant_id: OWNER_TENANT.to_string().into(),
                user_id: OWNER_USER.to_string().into(),
                client_id: None,
                session_type_id,
                enabled_capabilities: capabilities,
                metadata: None,
                lifecycle_state: LifecycleState::Active,
                share_token: None,
                created_at: now,
                updated_at: now,
            }),
        })
    }

    fn session_id(&self) -> Uuid {
        self.session.lock().session_id
    }
}

#[async_trait]
impl SessionRepo for MockSessionRepo {
    async fn insert(&self, _model: NewSession) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn find_by_id(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        let s = self.session.lock().clone();
        if s.tenant_id.as_str() == tenant_id
            && s.user_id.as_str() == user_id
            && s.session_id == session_id
        {
            Ok(Some(s))
        } else {
            Ok(None)
        }
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
        _id: Uuid,
        _m: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn update_capabilities(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _c: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn update_lifecycle_state(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _s: LifecycleState,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn soft_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _d: i64,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn hard_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
    ) -> std::result::Result<bool, ChatEngineError> {
        Ok(true)
    }

    // Scoped (Phase 4) surface used by the service under a permissive enforcer.
    // These unit mocks ignore the scope and return the single fixed session.
    async fn find_by_id_scoped(
        &self,
        _scope: &AccessScope,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        let s = self.session.lock().clone();
        Ok((s.session_id == session_id).then_some(s))
    }

    async fn update_metadata_scoped(
        &self,
        _scope: &AccessScope,
        _session_id: Uuid,
        _metadata: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }
}

struct MockSessionTypeRepo {
    st: Mutex<SessionType>,
}

impl MockSessionTypeRepo {
    fn new(session_type_id: Uuid, plugin_instance_id: Option<String>) -> Arc<Self> {
        let now = OffsetDateTime::now_utc();
        Arc::new(Self {
            st: Mutex::new(SessionType {
                session_type_id,
                name: "test".into(),
                plugin_instance_id,
                created_at: now,
                updated_at: now,
            }),
        })
    }
}

#[async_trait]
impl SessionTypeRepo for MockSessionTypeRepo {
    async fn insert(
        &self,
        _m: NewSessionType,
    ) -> std::result::Result<SessionType, ChatEngineError> {
        Ok(self.st.lock().clone())
    }

    async fn find_by_id(
        &self,
        session_type_id: Uuid,
    ) -> std::result::Result<Option<SessionType>, ChatEngineError> {
        let row = self.st.lock().clone();
        if row.session_type_id == session_type_id {
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn list(&self) -> std::result::Result<Vec<SessionType>, ChatEngineError> {
        Ok(vec![self.st.lock().clone()])
    }
}

#[derive(Default)]
struct MockMessageRepo {
    finalize_calls: Mutex<Vec<(Uuid, FinalizeOutcomeSnapshot)>>,
}

#[derive(Debug, Clone, PartialEq)]
enum FinalizeOutcomeSnapshot {
    Complete {
        text: String,
        metadata: Option<JsonValue>,
    },
    Cancelled {
        text: String,
    },
    Errored {
        text: String,
        error: String,
        finish_reason: String,
    },
}

impl From<FinalizeOutcome> for FinalizeOutcomeSnapshot {
    fn from(value: FinalizeOutcome) -> Self {
        match value {
            FinalizeOutcome::Complete { text, metadata, .. } => Self::Complete { text, metadata },
            FinalizeOutcome::Cancelled { text } => Self::Cancelled { text },
            FinalizeOutcome::Errored {
                text,
                error,
                finish_reason,
            } => Self::Errored {
                text,
                error,
                finish_reason: finish_reason.to_string(),
            },
        }
    }
}

#[async_trait]
impl MessageRepo for MockMessageRepo {
    async fn insert_user_and_assistant_stub(
        &self,
        req: NewUserMessage,
    ) -> std::result::Result<InsertedPair, ChatEngineError> {
        let _ = req;
        Ok(InsertedPair {
            user_message_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
            user_variant_index: 0,
        })
    }

    async fn finalize_assistant(
        &self,
        _session_id: Uuid,
        assistant_message_id: Uuid,
        outcome: FinalizeOutcome,
    ) -> std::result::Result<(), ChatEngineError> {
        self.finalize_calls
            .lock()
            .push((assistant_message_id, outcome.into()));
        Ok(())
    }

    async fn fetch_active_history(
        &self,
        _session_id: Uuid,
        _depth: Option<u32>,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(vec![])
    }

    async fn find_message_in_session(
        &self,
        _session_id: Uuid,
        _message_id: Uuid,
    ) -> std::result::Result<Option<Message>, ChatEngineError> {
        Ok(None)
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

/// Plugin scripted by a sequence of plugin-side outcomes.
enum PluginScript {
    Events(Vec<StreamingEvent>),
    PreError(PluginError),
    EventsThenErr(Vec<StreamingEvent>, PluginError),
    Hang, // never resolves; relies on cancellation
}

struct ScriptPlugin {
    id: String,
    script: Mutex<Option<PluginScript>>,
    calls: AtomicUsize,
}

impl ScriptPlugin {
    fn new(id: &str, script: PluginScript) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_owned(),
            script: Mutex::new(Some(script)),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl ChatEngineBackendPlugin for ScriptPlugin {
    async fn on_message(
        &self,
        _ctx: MessagePluginCtx,
    ) -> std::result::Result<PluginStream, PluginError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let script = self
            .script
            .lock()
            .take()
            .unwrap_or(PluginScript::Events(vec![]));
        match script {
            PluginScript::Events(events) => Ok(stream_from_events(events)),
            PluginScript::PreError(e) => Err(e),
            PluginScript::EventsThenErr(events, err) => {
                let mut items: Vec<std::result::Result<StreamingEvent, PluginError>> =
                    events.into_iter().map(Ok).collect();
                items.push(Err(err));
                Ok(futures::stream::iter(items).boxed())
            }
            PluginScript::Hang => {
                // A stream that yields nothing — cancellation is the
                // only way out.
                Ok(empty_stream_pending())
            }
        }
    }

    fn plugin_instance_id(&self) -> &str {
        &self.id
    }
}

/// Stream that yields `Pending` forever — used to test cancellation.
fn empty_stream_pending() -> PluginStream {
    futures::stream::poll_fn(|_cx| std::task::Poll::Pending).boxed()
}

// ----------------- Test fixtures -----------------

/// Owner pair carried by every fixture session in this module. [`make_ctx`]
/// builds a context for the same pair so the caller is the session owner —
/// what `owner_guard::ensure_session_owner` requires of an authorized op.
const OWNER_TENANT: Uuid = Uuid::from_u128(0x0A11);
const OWNER_USER: Uuid = Uuid::from_u128(0x0B22);

fn make_ctx() -> SecurityContext {
    test_support::ctx_for_subject(OWNER_USER, OWNER_TENANT)
}

fn make_service(
    plugin_id: &str,
    plugin: Arc<dyn ChatEngineBackendPlugin>,
    session_type_id: Uuid,
    capabilities: Option<JsonValue>,
) -> (MessageService, Arc<MockSessionRepo>, Arc<MockMessageRepo>) {
    let sessions = MockSessionRepo::new(Some(session_type_id), capabilities);
    let session_types = MockSessionTypeRepo::new(session_type_id, Some(plugin_id.to_owned()));
    let messages = Arc::new(MockMessageRepo::default());

    let hub = Arc::new(ClientHub::new());
    hub.register_scoped::<dyn ChatEngineBackendPlugin>(ClientScope::gts_id(plugin_id), plugin);
    let plugin_service = PluginService::new(hub, Arc::new(StubPluginConfigRepo));

    let svc = MessageService::new(
        sessions.clone() as Arc<dyn SessionRepo>,
        session_types as Arc<dyn SessionTypeRepo>,
        messages.clone() as Arc<dyn MessageRepo>,
        plugin_service,
        test_support::enforcer_allow(),
    );
    (svc, sessions, messages)
}

fn make_request(session_id: Uuid) -> SendMessageRequest {
    SendMessageRequest {
        session_id,
        parts: vec![MessagePartInput {
            part_type: chat_engine_sdk::models::MessagePartType::Text,
            content: serde_json::json!({"text": "hello"}),
            file_citations: vec![],
            link_citations: vec![],
            references: vec![],
        }],
        file_ids: vec![],
        parent_message_id: None,
        capabilities: None,
        metadata: None,
    }
}

// ----------------- Tests -----------------

#[tokio::test]
async fn happy_path_emits_start_chunks_complete() {
    let plugin_id = "plugin-happy";
    let session_type_id = Uuid::new_v4();
    let assistant_placeholder = Uuid::nil();
    let plugin = ScriptPlugin::new(
        plugin_id,
        PluginScript::Events(vec![
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: assistant_placeholder,
                chunk: "a".into(),
            }),
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: assistant_placeholder,
                chunk: "b".into(),
            }),
            StreamingEvent::Complete(StreamingCompleteEvent {
                message_id: assistant_placeholder,
                metadata: Some(serde_json::json!({"model": "test"})),
                file_citations: vec![],
                link_citations: vec![],
                references: vec![],
            }),
        ]),
    );
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, messages) = make_service(plugin_id, plugin_dyn, session_type_id, None);

    let req = make_request(sessions.session_id());
    let cancel = CancellationToken::new();
    let mut stream = svc
        .send_message(req, &make_ctx(), cancel)
        .await
        .expect("send_message dispatch");

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

    // Allow the spawned finalize to land.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let calls = messages.finalize_calls.lock().clone();
    assert_eq!(calls.len(), 1, "expected one finalize call");
    let (_id, outcome) = calls.into_iter().next().unwrap();
    match outcome {
        FinalizeOutcomeSnapshot::Complete { text, metadata } => {
            assert_eq!(text, "ab");
            assert_eq!(metadata, Some(serde_json::json!({"model": "test"})));
        }
        other => panic!("expected Complete finalize, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_stream_cancellation_finalizes_with_cancelled() {
    let plugin_id = "plugin-hang";
    let session_type_id = Uuid::new_v4();
    let plugin = ScriptPlugin::new(plugin_id, PluginScript::Hang);
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, messages) = make_service(plugin_id, plugin_dyn, session_type_id, None);

    let req = make_request(sessions.session_id());
    let cancel = CancellationToken::new();
    let mut stream = svc
        .send_message(req, &make_ctx(), cancel.clone())
        .await
        .expect("send_message dispatch");

    // First event must be Start.
    let evt = stream.next().await.expect("start event");
    assert!(matches!(evt, StreamingEvent::Start(_)));

    // Cancel mid-stream — the driver should exit.
    cancel.cancel();

    // Stream should now end (driver drops the sender).
    let next = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(
        matches!(next, Ok(None) | Err(_)),
        "stream must terminate after cancel"
    );

    tokio::time::sleep(Duration::from_millis(20)).await;
    let calls = messages.finalize_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    match &calls[0].1 {
        FinalizeOutcomeSnapshot::Cancelled { text } => assert_eq!(text, ""),
        other => panic!("expected Cancelled finalize, got {other:?}"),
    }
}

#[tokio::test]
async fn pre_stream_timeout_maps_to_backend_unavailable() {
    let plugin_id = "plugin-pre-timeout";
    let session_type_id = Uuid::new_v4();
    let plugin = ScriptPlugin::new(plugin_id, PluginScript::PreError(PluginError::timeout()));
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, messages) = make_service(plugin_id, plugin_dyn, session_type_id, None);

    let req = make_request(sessions.session_id());
    let cancel = CancellationToken::new();
    let result = svc.send_message(req, &make_ctx(), cancel).await;
    let err = match result {
        Ok(_) => panic!("pre-stream timeout must surface as Err"),
        Err(e) => e,
    };
    assert!(matches!(err, ChatEngineError::BackendUnavailable { .. }));

    // The assistant stub must have been finalised with finish_reason="timeout".
    let calls = messages.finalize_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    match &calls[0].1 {
        FinalizeOutcomeSnapshot::Errored {
            text,
            finish_reason,
            ..
        } => {
            assert!(text.is_empty());
            assert_eq!(finish_reason, "timeout");
        }
        other => panic!("expected Errored finalize, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_stream_err_emits_streaming_error_event_and_finalizes() {
    let plugin_id = "plugin-mid-err";
    let session_type_id = Uuid::new_v4();
    let assistant_placeholder = Uuid::nil();
    let plugin = ScriptPlugin::new(
        plugin_id,
        PluginScript::EventsThenErr(
            vec![StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: assistant_placeholder,
                chunk: "partial".into(),
            })],
            PluginError::internal("boom"),
        ),
    );
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, messages) = make_service(plugin_id, plugin_dyn, session_type_id, None);

    let req = make_request(sessions.session_id());
    let cancel = CancellationToken::new();
    let mut stream = svc
        .send_message(req, &make_ctx(), cancel)
        .await
        .expect("send_message dispatch");

    let mut got_error = false;
    let mut got_chunk = false;
    while let Some(evt) = stream.next().await {
        match evt {
            StreamingEvent::Chunk(_) => got_chunk = true,
            StreamingEvent::Error(e) => {
                got_error = true;
                assert!(e.error.contains("boom"));
            }
            _ => {}
        }
    }
    assert!(got_chunk, "expected at least one chunk");
    assert!(got_error, "expected a StreamingErrorEvent on the wire");

    tokio::time::sleep(Duration::from_millis(10)).await;
    let calls = messages.finalize_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    match &calls[0].1 {
        FinalizeOutcomeSnapshot::Errored {
            text,
            finish_reason,
            ..
        } => {
            assert_eq!(text, "partial");
            assert_eq!(finish_reason, "error");
        }
        other => panic!("expected Errored finalize, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_content_rejected_as_bad_request() {
    let plugin_id = "plugin-irrelevant";
    let session_type_id = Uuid::new_v4();
    let plugin = ScriptPlugin::new(plugin_id, PluginScript::Events(vec![]));
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, _messages) = make_service(plugin_id, plugin_dyn, session_type_id, None);

    let mut req = make_request(sessions.session_id());
    req.parts = vec![];
    let cancel = CancellationToken::new();
    let result = svc.send_message(req, &make_ctx(), cancel).await;
    let err = match result {
        Ok(_) => panic!("message with no parts must be rejected"),
        Err(e) => e,
    };
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn capability_not_in_session_rejected() {
    let plugin_id = "plugin-caps";
    let session_type_id = Uuid::new_v4();
    let plugin = ScriptPlugin::new(plugin_id, PluginScript::Events(vec![]));
    let plugin_dyn: Arc<dyn ChatEngineBackendPlugin> = plugin;
    let (svc, sessions, _messages) = make_service(
        plugin_id,
        plugin_dyn,
        session_type_id,
        Some(serde_json::json!([{"name": "allowed", "value": "x"}])),
    );

    let mut req = make_request(sessions.session_id());
    req.capabilities = Some(vec![CapabilityValue {
        name: "forbidden".into(),
        value: serde_json::json!(true),
    }]);
    let cancel = CancellationToken::new();
    let result = svc.send_message(req, &make_ctx(), cancel).await;
    let err = match result {
        Ok(_) => panic!("disallowed capability must be rejected"),
        Err(e) => e,
    };
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[test]
fn finish_reason_for_maps_variants() {
    assert_eq!(finish_reason_for(&PluginError::timeout()), "timeout");
    assert_eq!(
        finish_reason_for(&PluginError::transient("x")),
        "interrupted"
    );
    assert_eq!(
        finish_reason_for(&PluginError::rate_limited(None)),
        "interrupted"
    );
    assert_eq!(finish_reason_for(&PluginError::internal("x")), "error");
}

// ============================================================
// Phase 7 — context management tests
// ============================================================

use chat_engine_sdk::models::{
    MessagePart, MessageRole, TenantId as SdkTenantId, UserId as SdkUserId,
};

/// Mock `MessageRepo` whose `list_active_path` returns a caller-supplied
/// sequence — lets `apply_memory_strategy` tests stay in-process.
struct ScriptedMessageRepo {
    active_path: Mutex<Vec<Message>>,
}

impl ScriptedMessageRepo {
    fn new(active: Vec<Message>) -> Arc<Self> {
        Arc::new(Self {
            active_path: Mutex::new(active),
        })
    }
}

#[async_trait]
impl MessageRepo for ScriptedMessageRepo {
    async fn insert_user_and_assistant_stub(
        &self,
        _req: NewUserMessage,
    ) -> std::result::Result<InsertedPair, ChatEngineError> {
        Ok(InsertedPair {
            user_message_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
            user_variant_index: 0,
        })
    }

    async fn finalize_assistant(
        &self,
        _session_id: Uuid,
        _id: Uuid,
        _outcome: FinalizeOutcome,
    ) -> std::result::Result<(), ChatEngineError> {
        Ok(())
    }

    async fn fetch_active_history(
        &self,
        _session_id: Uuid,
        _depth: Option<u32>,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(self
            .active_path
            .lock()
            .iter()
            .filter(|m| !m.is_hidden_from_backend)
            .cloned()
            .collect())
    }

    async fn find_message_in_session(
        &self,
        _session_id: Uuid,
        _message_id: Uuid,
    ) -> std::result::Result<Option<Message>, ChatEngineError> {
        Ok(None)
    }

    async fn list_active_path(
        &self,
        _session_id: Uuid,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(self.active_path.lock().clone())
    }
}

/// Build a `Message` fixture with the fields the strategy algorithm
/// inspects (`is_active`, `is_hidden_from_backend`, `created_at`).
fn make_message(idx: usize, hidden: bool) -> Message {
    Message {
        message_id: Uuid::new_v4(),
        session_id: Uuid::nil(),
        tenant_id: None,
        user_id: None,
        parent_message_id: None,
        variant_index: 0,
        is_active: true,
        role: MessageRole::User,
        parts: vec![MessagePart::text(
            Uuid::nil(),
            Uuid::nil(),
            0,
            format!("msg-{idx}"),
        )],
        file_ids: vec![],
        metadata: None,
        is_complete: true,
        is_hidden_from_user: false,
        is_hidden_from_backend: hidden,
        created_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(idx as u64),
        updated_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(idx as u64),
    }
}

/// Build a current-user `Message` fixture appended last by the strategy.
fn make_current_message() -> Message {
    Message {
        message_id: Uuid::new_v4(),
        session_id: Uuid::nil(),
        tenant_id: None,
        user_id: None,
        parent_message_id: None,
        variant_index: 0,
        is_active: true,
        role: MessageRole::User,
        parts: vec![MessagePart::text(Uuid::nil(), Uuid::nil(), 0, "CURRENT")],
        file_ids: vec![],
        metadata: None,
        is_complete: true,
        is_hidden_from_user: false,
        is_hidden_from_backend: false,
        created_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1000),
        updated_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1000),
    }
}

/// Build a `Session` fixture with an explicit metadata payload.
fn make_session(metadata: Option<JsonValue>) -> Session {
    Session {
        session_id: Uuid::new_v4(),
        tenant_id: SdkTenantId::new(OWNER_TENANT.to_string()),
        user_id: SdkUserId::new(OWNER_USER.to_string()),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// Construct a `MessageService` against a scripted message repo + the
/// stock session repo. Plugin and session-type repos are unused for the
/// strategy-algorithm tests.
fn make_strategy_service(active_path: Vec<Message>) -> (MessageService, Arc<ScriptedMessageRepo>) {
    let messages = ScriptedMessageRepo::new(active_path);
    let sessions = MockSessionRepo::new(None, None);
    let session_types = MockSessionTypeRepo::new(Uuid::new_v4(), None);
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    let svc = MessageService::new(
        sessions as Arc<dyn SessionRepo>,
        session_types as Arc<dyn SessionTypeRepo>,
        messages.clone() as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    );
    (svc, messages)
}

#[tokio::test]
async fn apply_strategy_full_defaults_when_metadata_absent() {
    let active = vec![
        make_message(0, false),
        make_message(1, false),
        make_message(2, false),
    ];
    let (svc, _repo) = make_strategy_service(active.clone());
    let session = make_session(None);
    let current = make_current_message();
    let out = svc
        .apply_memory_strategy(&session, &current)
        .await
        .expect("apply_memory_strategy default");
    assert_eq!(out.len(), 4, "3 visible + current");
    assert_eq!(out.last().unwrap().message_id, current.message_id);
}

#[tokio::test]
async fn apply_strategy_full_filters_hidden_messages() {
    let active = vec![
        make_message(0, false),
        make_message(1, true), // hidden
        make_message(2, false),
    ];
    let (svc, _repo) = make_strategy_service(active);
    let session = make_session(Some(serde_json::json!({
        "memory_strategy": {"type": "full"},
    })));
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();
    // 2 visible + current = 3
    assert_eq!(out.len(), 3);
    // Hidden message must not appear in the prefix.
    assert!(!out[..2].iter().any(|m| m.is_hidden_from_backend));
    assert_eq!(out.last().unwrap().message_id, current.message_id);
}

#[tokio::test]
async fn apply_strategy_sliding_window_takes_last_n_visible() {
    let active = vec![
        make_message(0, false),
        make_message(1, false),
        make_message(2, false),
        make_message(3, false),
        make_message(4, false),
    ];
    let (svc, _repo) = make_strategy_service(active.clone());
    let session = make_session(Some(serde_json::json!({
        "memory_strategy": {"type": "sliding_window", "window_size": 2},
    })));
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();
    // Last 2 + current = 3.
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_id, active[3].message_id);
    assert_eq!(out[1].message_id, active[4].message_id);
    assert_eq!(out[2].message_id, current.message_id);
}

#[tokio::test]
async fn apply_strategy_sliding_window_window_larger_than_visible_uses_all() {
    let active = vec![make_message(0, false), make_message(1, false)];
    let (svc, _repo) = make_strategy_service(active);
    let session = make_session(Some(serde_json::json!({
        "memory_strategy": {"type": "sliding_window", "window_size": 50},
    })));
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();
    // 2 visible + current = 3.
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn apply_strategy_summarized_keeps_last_k_regardless_of_visibility() {
    // 5 active path messages: indices 0,1 visible; 2,3 hidden; 4 visible.
    // recent_messages_to_keep=2 → indices 3 and 4 are the last-K.
    // Result must include:
    //   - index 0 (visible)
    //   - index 1 (visible)
    //   - index 3 (last-K, hidden but kept)
    //   - index 4 (last-K + visible)
    // Index 2 (hidden, not in last-K) must be excluded.
    let active = vec![
        make_message(0, false),
        make_message(1, false),
        make_message(2, true),
        make_message(3, true),
        make_message(4, false),
    ];
    let (svc, _repo) = make_strategy_service(active.clone());
    let session = make_session(Some(serde_json::json!({
        "memory_strategy": {"type": "summarized", "recent_messages_to_keep": 2},
    })));
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();

    let ids: Vec<Uuid> = out.iter().map(|m| m.message_id).collect();
    assert_eq!(ids.len(), 5, "4 selected + current");
    assert_eq!(ids[0], active[0].message_id);
    assert_eq!(ids[1], active[1].message_id);
    assert_eq!(ids[2], active[3].message_id);
    assert_eq!(ids[3], active[4].message_id);
    assert_eq!(ids[4], current.message_id);
    // Index 2 must NOT appear.
    assert!(!ids.contains(&active[2].message_id));
}

#[tokio::test]
async fn apply_strategy_appends_current_msg_last() {
    let active = vec![make_message(0, false)];
    let (svc, _repo) = make_strategy_service(active);
    let session = make_session(None);
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();
    assert_eq!(out.last().unwrap().message_id, current.message_id);
}

#[tokio::test]
async fn apply_strategy_summarized_handles_keep_greater_than_active() {
    let active = vec![make_message(0, true), make_message(1, true)];
    let (svc, _repo) = make_strategy_service(active.clone());
    let session = make_session(Some(serde_json::json!({
        "memory_strategy": {"type": "summarized", "recent_messages_to_keep": 100},
    })));
    let current = make_current_message();
    let out = svc.apply_memory_strategy(&session, &current).await.unwrap();
    // recent_start = saturating_sub(2, 100) = 0 → all messages kept
    // regardless of hidden flag.
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_id, active[0].message_id);
    assert_eq!(out[1].message_id, active[1].message_id);
    assert_eq!(out[2].message_id, current.message_id);
}

// ---- handle_context_overflow -----------------------------------

#[tokio::test]
async fn handle_overflow_full_propagates_as_backend_unavailable() {
    let (svc, _repo) = make_strategy_service(vec![]);
    let err = svc
        .handle_context_overflow("t", "u", Uuid::new_v4(), &MemoryStrategy::Full)
        .await
        .expect_err("full propagates overflow");
    assert!(matches!(err, ChatEngineError::BackendUnavailable { .. }));
}

#[tokio::test]
async fn handle_overflow_sliding_window_propagates() {
    let (svc, _repo) = make_strategy_service(vec![]);
    let err = svc
        .handle_context_overflow(
            "t",
            "u",
            Uuid::new_v4(),
            &MemoryStrategy::SlidingWindow { window_size: 5 },
        )
        .await
        .expect_err("sliding window propagates overflow");
    assert!(matches!(err, ChatEngineError::BackendUnavailable { .. }));
}

#[tokio::test]
async fn handle_overflow_summarized_skips_when_session_inaccessible() {
    // Recovery now goes through the SCOPED `find_by_id` — the
    // strategy-test mock SessionRepo only returns rows when the
    // tenant/user match its single seeded row. Passing identity
    // values that do not match (`"t"` / `"u"` here vs the mock's
    // synthetic `"t"` / `"u"` from `MockSessionRepo::new`) means
    // the lookup may surface a row OR return None depending on the
    // fixture's identity. Either way the contract is: NO panic, NO
    // cross-scope leak.
    let (svc, _repo) = make_strategy_service(vec![]);
    svc.handle_context_overflow(
        "t",
        "u",
        Uuid::new_v4(),
        &MemoryStrategy::Summarized {
            recent_messages_to_keep: 3,
        },
    )
    .await
    .expect("missing session degrades gracefully under scoped lookup");
}

// ---- update_memory_strategy (PATCH /sessions/{id}) ----------------

/// `MockSessionRepo` extension shim: replace the held row with a
/// pre-baked lifecycle state, then validate update_memory_strategy
/// against it.
fn make_repo_with_state(state: &str) -> Arc<MockSessionRepo> {
    let repo = MockSessionRepo::new(None, None);
    repo.session.lock().lifecycle_state =
        LifecycleState::from_str_value(state).unwrap_or(LifecycleState::Active);
    repo
}

fn make_service_with_session_repo(sessions: Arc<MockSessionRepo>) -> MessageService {
    let session_types = MockSessionTypeRepo::new(Uuid::new_v4(), None);
    let messages = Arc::new(MockMessageRepo::default());
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    MessageService::new(
        sessions as Arc<dyn SessionRepo>,
        session_types as Arc<dyn SessionTypeRepo>,
        messages as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    )
}

#[tokio::test]
async fn update_strategy_rejects_invalid_window() {
    let repo = make_repo_with_state("active");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    let err = svc
        .update_memory_strategy(
            &make_ctx(),
            session_id,
            MemoryStrategy::SlidingWindow { window_size: 0 },
        )
        .await
        .expect_err("window_size=0 rejected");
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn update_strategy_rejects_summarized_below_two() {
    let repo = make_repo_with_state("active");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    let err = svc
        .update_memory_strategy(
            &make_ctx(),
            session_id,
            MemoryStrategy::Summarized {
                recent_messages_to_keep: 1,
            },
        )
        .await
        .expect_err("recent_messages_to_keep=1 rejected");
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn update_strategy_rejects_soft_deleted_session() {
    let repo = make_repo_with_state("soft_deleted");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    let err = svc
        .update_memory_strategy(&make_ctx(), session_id, MemoryStrategy::Full)
        .await
        .expect_err("soft_deleted rejected as 409");
    assert!(matches!(err, ChatEngineError::Conflict { .. }));
}

#[tokio::test]
async fn update_strategy_rejects_hard_deleted_session() {
    let repo = make_repo_with_state("hard_deleted");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    let err = svc
        .update_memory_strategy(&make_ctx(), session_id, MemoryStrategy::Full)
        .await
        .expect_err("hard_deleted rejected as 409");
    assert!(matches!(err, ChatEngineError::Conflict { .. }));
}

#[tokio::test]
async fn update_strategy_accepts_active_session() {
    let repo = make_repo_with_state("active");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    svc.update_memory_strategy(
        &make_ctx(),
        session_id,
        MemoryStrategy::SlidingWindow { window_size: 4 },
    )
    .await
    .expect("active session accepts strategy update");
}

#[tokio::test]
async fn update_strategy_accepts_archived_session() {
    let repo = make_repo_with_state("archived");
    let session_id = repo.session_id();
    let svc = make_service_with_session_repo(repo);
    svc.update_memory_strategy(&make_ctx(), session_id, MemoryStrategy::Full)
        .await
        .expect("archived session accepts strategy update");
}

#[test]
fn strategy_type_label_covers_all_variants() {
    assert_eq!(strategy_type_label(&MemoryStrategy::Full), "full");
    assert_eq!(
        strategy_type_label(&MemoryStrategy::SlidingWindow { window_size: 1 }),
        "sliding_window"
    );
    assert_eq!(
        strategy_type_label(&MemoryStrategy::Summarized {
            recent_messages_to_keep: 2
        }),
        "summarized"
    );
}

// ============================================================
// Phase 12 — delete_message_cascade tests
// ============================================================
//
// The fixtures below mirror Phase 5 / Phase 7 style: in-memory
// SessionRepo + MessageRepo carrying a session row and a tree of
// messages, allowing cross-tenant / cross-user / root / cascade
// scenarios without a live database. The Phase 1 reaction
// FK CASCADE is exercised implicitly by the in-memory
// `DeleteRepo::delete_message_subtree` impl, which clears reactions
// recorded against any subtree id.

use std::collections::HashSet;

/// In-memory SessionRepo for the Phase 12 delete tests. Stores a
/// single session row with an explicit `tenant_id` / `user_id` so
/// cross-tenant + cross-user scenarios can target it precisely.
struct DeleteSessionRepo {
    row: Mutex<Session>,
}

impl DeleteSessionRepo {
    fn new(tenant_id: &str, user_id: &str) -> Arc<Self> {
        let now = OffsetDateTime::now_utc();
        Arc::new(Self {
            row: Mutex::new(Session {
                session_id: Uuid::new_v4(),
                tenant_id: tenant_id.into(),
                user_id: user_id.into(),
                client_id: None,
                session_type_id: None,
                enabled_capabilities: None,
                metadata: None,
                lifecycle_state: LifecycleState::Active,
                share_token: None,
                created_at: now,
                updated_at: now,
            }),
        })
    }

    fn session_id(&self) -> Uuid {
        self.row.lock().session_id
    }
}

#[async_trait]
impl SessionRepo for DeleteSessionRepo {
    async fn insert(&self, _m: NewSession) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.row.lock().clone())
    }

    async fn find_by_id(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        let s = self.row.lock().clone();
        if s.tenant_id.as_str() == tenant_id
            && s.user_id.as_str() == user_id
            && s.session_id == session_id
        {
            Ok(Some(s))
        } else {
            Ok(None)
        }
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
        _id: Uuid,
        _m: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.row.lock().clone())
    }

    async fn update_capabilities(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _c: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.row.lock().clone())
    }

    async fn update_lifecycle_state(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _s: LifecycleState,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.row.lock().clone())
    }

    async fn soft_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
        _d: i64,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.row.lock().clone())
    }

    async fn hard_delete(
        &self,
        _t: &str,
        _u: &str,
        _id: Uuid,
    ) -> std::result::Result<bool, ChatEngineError> {
        Ok(true)
    }

    async fn find_by_session_id_unscoped(
        &self,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        let s = self.row.lock().clone();
        if s.session_id == session_id {
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }
}

/// In-memory MessageRepo for the Phase 12 delete tests. Stores a
/// fixed map of messages keyed by id and a side-table of reactions
/// (one bool per message id). The cascade `delete_message_subtree`
/// implementation walks `parent_message_id`, deletes leaves-first,
/// and drops the reactions for every removed id — emulating the
/// Postgres FK CASCADE we rely on in production.
struct DeleteMessageRepo {
    messages: Mutex<Vec<Message>>,
    reactions: Mutex<HashSet<Uuid>>,
}

impl DeleteMessageRepo {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            messages: Mutex::new(Vec::new()),
            reactions: Mutex::new(HashSet::new()),
        })
    }

    fn insert(&self, msg: Message) {
        self.messages.lock().push(msg);
    }

    fn record_reaction(&self, message_id: Uuid) {
        self.reactions.lock().insert(message_id);
    }

    fn message_count(&self) -> usize {
        self.messages.lock().len()
    }

    fn reaction_count(&self) -> usize {
        self.reactions.lock().len()
    }

    fn has_message(&self, id: Uuid) -> bool {
        self.messages.lock().iter().any(|m| m.message_id == id)
    }

    fn has_reaction(&self, id: Uuid) -> bool {
        self.reactions.lock().contains(&id)
    }
}

#[async_trait]
impl MessageRepo for DeleteMessageRepo {
    async fn insert_user_and_assistant_stub(
        &self,
        _req: NewUserMessage,
    ) -> std::result::Result<InsertedPair, ChatEngineError> {
        Ok(InsertedPair {
            user_message_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
            user_variant_index: 0,
        })
    }

    async fn finalize_assistant(
        &self,
        _session_id: Uuid,
        _id: Uuid,
        _o: FinalizeOutcome,
    ) -> std::result::Result<(), ChatEngineError> {
        Ok(())
    }

    async fn fetch_active_history(
        &self,
        _s: Uuid,
        _d: Option<u32>,
    ) -> std::result::Result<Vec<Message>, ChatEngineError> {
        Ok(vec![])
    }

    async fn find_message_in_session(
        &self,
        session_id: Uuid,
        message_id: Uuid,
    ) -> std::result::Result<Option<Message>, ChatEngineError> {
        Ok(self
            .messages
            .lock()
            .iter()
            .find(|m| m.message_id == message_id && m.session_id == session_id)
            .cloned())
    }

    async fn delete_message_subtree(
        &self,
        session_id: Uuid,
        root_id: Uuid,
    ) -> std::result::Result<u64, ChatEngineError> {
        // Collect descendants iteratively from `parent_message_id`.
        let mut to_visit: Vec<Uuid> = vec![root_id];
        let mut ordered: Vec<Uuid> = Vec::new();
        {
            let messages = self.messages.lock();
            while let Some(id) = to_visit.pop() {
                if !messages
                    .iter()
                    .any(|m| m.message_id == id && m.session_id == session_id)
                {
                    // Idempotent: a missing root contributes 0.
                    continue;
                }
                ordered.push(id);
                for child in messages
                    .iter()
                    .filter(|m| m.session_id == session_id && m.parent_message_id == Some(id))
                    .map(|m| m.message_id)
                {
                    to_visit.push(child);
                }
            }
        }

        // Delete leaves-first to mirror the Phase 8 primitive ordering.
        let mut removed: u64 = 0;
        let removed_set: HashSet<Uuid> = ordered.iter().copied().collect();
        {
            let mut messages = self.messages.lock();
            messages.retain(|m| {
                let keep = !(m.session_id == session_id && removed_set.contains(&m.message_id));
                if !keep {
                    removed += 1;
                }
                keep
            });
        }
        // FK CASCADE emulation: drop reactions for every removed id.
        {
            let mut reactions = self.reactions.lock();
            reactions.retain(|id| !removed_set.contains(id));
        }
        Ok(removed)
    }

    // Scoped (Phase 4) surface: delete_message_cascade runs under a permissive
    // enforcer here, so the mock delegates to the unscoped logic above.
    async fn find_message_in_session_scoped(
        &self,
        _scope: &AccessScope,
        session_id: Uuid,
        message_id: Uuid,
    ) -> std::result::Result<Option<Message>, ChatEngineError> {
        self.find_message_in_session(session_id, message_id).await
    }

    async fn delete_message_subtree_scoped(
        &self,
        _scope: &AccessScope,
        session_id: Uuid,
        root_id: Uuid,
    ) -> std::result::Result<u64, ChatEngineError> {
        self.delete_message_subtree(session_id, root_id).await
    }
}

/// Snapshot webhook emitter that records every emitted event in a
/// shared `Vec`. Used to assert that `delete_message_cascade` fires
/// the `message.deleted` event AFTER commit on success and NEVER on
/// the failure paths.
#[derive(Default)]
struct RecordingEmitter {
    events: Mutex<Vec<WebhookEvent>>,
}

impl RecordingEmitter {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn snapshot(&self) -> Vec<WebhookEvent> {
        self.events.lock().clone()
    }
}

#[async_trait]
impl WebhookEmitter for RecordingEmitter {
    async fn emit(&self, event: WebhookEvent) -> Result<()> {
        self.events.lock().push(event);
        Ok(())
    }
}

/// Build a `Message` row that lives inside `session_id` with the
/// given parent. The variant-index / lifecycle bits are unused by
/// the delete path so we keep them at sensible defaults.
fn delete_message_row(session_id: Uuid, parent: Option<Uuid>) -> Message {
    Message {
        message_id: Uuid::new_v4(),
        session_id,
        tenant_id: None,
        user_id: None,
        parent_message_id: parent,
        variant_index: 0,
        is_active: true,
        role: MessageRole::User,
        parts: vec![MessagePart::text(Uuid::nil(), Uuid::nil(), 0, "x")],
        file_ids: vec![],
        metadata: None,
        is_complete: true,
        is_hidden_from_user: false,
        is_hidden_from_backend: false,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

/// Composite Phase-12 test fixture. Returns the service, the session
/// repo (so tests can read the session id back), the message repo,
/// and the recording webhook emitter.
fn make_delete_fixture(
    tenant_id: &str,
    user_id: &str,
) -> (
    MessageService,
    Arc<DeleteSessionRepo>,
    Arc<DeleteMessageRepo>,
    Arc<RecordingEmitter>,
) {
    let sessions = DeleteSessionRepo::new(tenant_id, user_id);
    let messages = DeleteMessageRepo::new();
    let session_types = MockSessionTypeRepo::new(Uuid::new_v4(), None);
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::new(StubPluginConfigRepo));
    let webhooks = RecordingEmitter::new();
    let svc = MessageService::new(
        sessions.clone() as Arc<dyn SessionRepo>,
        session_types as Arc<dyn SessionTypeRepo>,
        messages.clone() as Arc<dyn MessageRepo>,
        plugins,
        test_support::enforcer_allow(),
    )
    .with_webhook_emitter(webhooks.clone() as Arc<dyn WebhookEmitter>);
    (svc, sessions, messages, webhooks)
}

/// Wait for the detached webhook task to drain. The emit task is
/// spawned via `tokio::spawn`; a short polling loop is the
/// deterministic equivalent of awaiting its JoinHandle without
/// having to reach into the service.
async fn drain_webhooks(emitter: &RecordingEmitter, expected: usize) {
    for _ in 0..50 {
        if emitter.snapshot().len() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn delete_cascade_happy_path_removes_subtree_and_reactions() {
    let (svc, sessions, messages, webhooks) = make_delete_fixture("t", "u");
    let session_id = sessions.session_id();

    // Tree shape:
    //   root  (id=r)
    //   ├── target (id=t)
    //   │     ├── grandchild_a (id=ga)
    //   │     └── grandchild_b (id=gb)
    //   └── sibling (id=s)        — must survive the delete.
    let root = delete_message_row(session_id, None);
    let target = delete_message_row(session_id, Some(root.message_id));
    let grandchild_a = delete_message_row(session_id, Some(target.message_id));
    let grandchild_b = delete_message_row(session_id, Some(target.message_id));
    let sibling = delete_message_row(session_id, Some(root.message_id));

    let target_id = target.message_id;
    let grandchild_a_id = grandchild_a.message_id;
    let grandchild_b_id = grandchild_b.message_id;
    let sibling_id = sibling.message_id;
    let root_id = root.message_id;

    for msg in [&root, &target, &grandchild_a, &grandchild_b, &sibling] {
        messages.insert(msg.clone());
    }
    // One reaction per node — only the three in the target subtree
    // should be removed.
    for id in [
        root_id,
        target_id,
        grandchild_a_id,
        grandchild_b_id,
        sibling_id,
    ] {
        messages.record_reaction(id);
    }

    assert_eq!(messages.message_count(), 5);
    assert_eq!(messages.reaction_count(), 5);

    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let outcome = svc
        .delete_message_cascade(
            &test_support::ctx_for_subject(user, tenant),
            session_id,
            target_id,
        )
        .await
        .expect("happy path cascade");

    assert_eq!(outcome.message_id, target_id);
    assert_eq!(outcome.deleted_count, 3, "target + 2 grandchildren");
    // Subtree gone, sibling + root survive.
    assert!(!messages.has_message(target_id));
    assert!(!messages.has_message(grandchild_a_id));
    assert!(!messages.has_message(grandchild_b_id));
    assert!(messages.has_message(root_id));
    assert!(messages.has_message(sibling_id));
    // Reactions for the removed subtree gone; root + sibling intact.
    assert!(!messages.has_reaction(target_id));
    assert!(!messages.has_reaction(grandchild_a_id));
    assert!(!messages.has_reaction(grandchild_b_id));
    assert!(messages.has_reaction(root_id));
    assert!(messages.has_reaction(sibling_id));

    // Webhook fired post-commit.
    drain_webhooks(&webhooks, 1).await;
    let events = webhooks.snapshot();
    assert_eq!(events.len(), 1);
    match &events[0] {
        WebhookEvent::MessageDeleted {
            session_id: ev_session,
            message_id: ev_msg,
            tenant_id,
            user_id,
            deleted_count,
            ..
        } => {
            assert_eq!(*ev_session, session_id);
            assert_eq!(*ev_msg, target_id);
            assert_eq!(tenant_id, &tenant.to_string());
            assert_eq!(user_id, &user.to_string());
            assert_eq!(*deleted_count, 3);
        }
        other => panic!("expected MessageDeleted, got {other:?}"),
    }
    assert_eq!(events[0].kind(), "message.deleted");
}

#[tokio::test]
async fn delete_root_returns_conflict_without_writes() {
    let (svc, sessions, messages, webhooks) = make_delete_fixture("t", "u");
    let session_id = sessions.session_id();

    let root = delete_message_row(session_id, None);
    let child = delete_message_row(session_id, Some(root.message_id));
    let root_id = root.message_id;
    let child_id = child.message_id;
    messages.insert(root);
    messages.insert(child);

    let err = svc
        .delete_message_cascade(&make_ctx(), session_id, root_id)
        .await
        .expect_err("root delete must 409");
    assert!(matches!(err, ChatEngineError::Conflict { .. }));

    // No DB mutation.
    assert!(messages.has_message(root_id));
    assert!(messages.has_message(child_id));

    // No webhook emitted on failure.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        webhooks.snapshot().is_empty(),
        "webhook must not fire on root-delete failure"
    );
}

// NOTE: the old `delete_cross_tenant_returns_forbidden` /
// `delete_cross_user_same_tenant_returns_not_found` tests asserted the
// pre-Phase-4 `check_session_scope` behavior (403/404), which no longer exists —
// delete_message_cascade now gates via access_scope(MESSAGE, delete) and a
// scoped find. The cross-tenant → 404 semantics are covered by
// `delete_message_wrong_tenant_returns_not_found` in the real-DB authz suite
// below.

#[tokio::test]
async fn delete_missing_message_returns_not_found() {
    let (svc, sessions, messages, webhooks) = make_delete_fixture("t", "u");
    let session_id = sessions.session_id();
    // No messages inserted — target id resolves to nothing.
    let phantom_id = Uuid::new_v4();
    let err = svc
        .delete_message_cascade(&make_ctx(), session_id, phantom_id)
        .await
        .expect_err("missing message must 404");
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
    assert_eq!(messages.message_count(), 0);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(webhooks.snapshot().is_empty());
}

#[tokio::test]
async fn delete_idempotent_re_delete_returns_not_found() {
    let (svc, sessions, messages, webhooks) = make_delete_fixture("t", "u");
    let session_id = sessions.session_id();

    let root = delete_message_row(session_id, None);
    let target = delete_message_row(session_id, Some(root.message_id));
    let target_id = target.message_id;
    messages.insert(root);
    messages.insert(target);

    // First delete succeeds.
    let outcome = svc
        .delete_message_cascade(&make_ctx(), session_id, target_id)
        .await
        .expect("first delete succeeds");
    assert_eq!(outcome.deleted_count, 1);

    // Second delete — target no longer exists → 404.
    let err = svc
        .delete_message_cascade(&make_ctx(), session_id, target_id)
        .await
        .expect_err("re-delete must 404");
    assert!(matches!(err, ChatEngineError::NotFound { .. }));

    // Only the first delete fires a webhook.
    drain_webhooks(&webhooks, 1).await;
    let events = webhooks.snapshot();
    assert_eq!(
        events.len(),
        1,
        "exactly one webhook for the successful delete"
    );
}

#[tokio::test]
async fn delete_missing_session_returns_not_found() {
    let (svc, _sessions, _messages, webhooks) = make_delete_fixture("t", "u");
    let phantom_session = Uuid::new_v4();
    let phantom_message = Uuid::new_v4();
    let err = svc
        .delete_message_cascade(&make_ctx(), phantom_session, phantom_message)
        .await
        .expect_err("missing session must 404");
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(webhooks.snapshot().is_empty());
}

// ===========================================================================
// Authorization suite (Phase 8) — real Sea-ORM repos over in-memory SQLite +
// mock PDP, exercising the enforcer -> AccessScope -> SecureORM flow.
// ===========================================================================

/// Seed a session + a (user, assistant) message pair via the real repo. The
/// child owner pair is inherited from the session in the same transaction
/// (internal_write_scope path). Returns the assistant (non-root) message id.
async fn seed_session_with_message(
    db: &std::sync::Arc<crate::infra::db::repo::ChatEngineDb>,
    tenant: Uuid,
    user: Uuid,
) -> (Uuid, InsertedPair) {
    let session_id = Uuid::new_v4();
    test_support::seed_session(db, session_id, tenant, user).await;
    let ins = test_support::message_repo(db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: None,
            parts: vec![MessagePartInput {
                part_type: chat_engine_sdk::models::MessagePartType::Text,
                content: serde_json::json!({ "text": "hi" }),
                file_citations: vec![],
                link_citations: vec![],
                references: vec![],
            }],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed message");
    (session_id, ins)
}

// --- PDP deny (Denied) -> 403 ---------------------------------------------

#[tokio::test]
async fn pdp_denied_list_messages_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let svc = test_support::build_message_service(&db, test_support::enforcer_deny());
    let err = svc
        .list_active_messages(&make_ctx(), Uuid::new_v4(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_get_message_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let svc = test_support::build_message_service(&db, test_support::enforcer_deny());
    let err = svc
        .resolve_owned_message(&make_ctx(), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_delete_message_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let svc = test_support::build_message_service(&db, test_support::enforcer_deny());
    let err = svc
        .delete_message_cascade(&make_ctx(), Uuid::new_v4(), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_send_message_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let session_id = Uuid::new_v4();
    test_support::seed_session(&db, session_id, tenant, user).await;

    let svc = test_support::build_message_service(&db, test_support::enforcer_deny());
    // send_message returns a non-Debug BoxStream on Ok, so match instead of unwrap_err.
    let res = svc
        .send_message(
            make_request(session_id),
            &test_support::ctx_for_subject(user, tenant),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(res, Err(ChatEngineError::Forbidden { .. })),
        "expected Forbidden from DenyAll on send"
    );
}

// --- EvaluationFailed / CompileFailed -> 403 (fail-closed) ----------------

#[tokio::test]
async fn evaluation_failed_list_messages_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let svc = test_support::build_message_service(&db, test_support::enforcer_failing());
    let err = svc
        .list_active_messages(&make_ctx(), Uuid::new_v4(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn compile_failed_list_messages_returns_forbidden() {
    let db = test_support::inmem_db().await;
    let svc = test_support::build_message_service(&db, test_support::enforcer_compile_fail());
    let err = svc
        .list_active_messages(&make_ctx(), Uuid::new_v4(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "got: {err:?}"
    );
}

// --- Point-op scope-miss -> 404 (anti-enumeration) ------------------------

#[tokio::test]
async fn get_message_wrong_tenant_returns_not_found() {
    let db = test_support::inmem_db().await;
    let (_sid, ins) = seed_session_with_message(&db, Uuid::new_v4(), Uuid::new_v4()).await;

    let svc = test_support::build_message_service(&db, test_support::enforcer_allow());
    let err = svc
        .resolve_owned_message(
            &test_support::ctx_allow_tenants(&[Uuid::new_v4()]),
            ins.user_message_id,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "cross-tenant message read must 404, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_message_wrong_tenant_returns_not_found() {
    let db = test_support::inmem_db().await;
    let (session_id, ins) = seed_session_with_message(&db, Uuid::new_v4(), Uuid::new_v4()).await;

    let svc = test_support::build_message_service(&db, test_support::enforcer_allow());
    let err = svc
        .delete_message_cascade(
            &test_support::ctx_allow_tenants(&[Uuid::new_v4()]),
            session_id,
            ins.assistant_message_id,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "cross-tenant message delete must 404, got: {err:?}"
    );
}

// --- Denormalization invariant --------------------------------------------

// @cpt-cf-chat-engine-principle-owner-denorm-invariant
#[tokio::test]
async fn internal_write_copies_owner_pair_from_session() {
    use crate::infra::db::entity::message::{self, Entity as MessageEntity};
    use sea_orm::{ColumnTrait, Condition, EntityTrait};
    use toolkit_db::secure::{AccessScope, SecureEntityExt};

    let db = test_support::inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let (_session_id, ins) = seed_session_with_message(&db, tenant, user).await;

    // The raw DatabaseConnection is hidden by the secure layer, so verify the
    // stored owner pair via a tenant-scoped secure read on the entity model:
    // the row is returned ONLY if owner_tenant_id == tenant (tenant half), then
    // assert owner_id == session.user_id (owner half).
    let conn = db.conn().expect("conn");
    let scope = AccessScope::for_tenant(tenant);
    let row = MessageEntity::find()
        .secure()
        .scope_with(&scope)
        .filter(Condition::all().add(message::Column::MessageId.eq(ins.user_message_id)))
        .one(&conn)
        .await
        .expect("query message")
        .expect("message row visible under session tenant scope");
    assert_eq!(
        row.owner_tenant_id, tenant,
        "child inherits session owner_tenant_id"
    );
    assert_eq!(row.owner_id, user, "child inherits session owner_id");
}

// ===========================================================================
// Real-repo harness suite: exercise resolve/list/delete flows end-to-end so
// the scoped message_repo methods (find_message_by_id_scoped,
// fetch_active_history_scoped, find_message_in_session_scoped,
// delete_message_subtree_scoped) run under a permissive / denying PDP.
// ===========================================================================

use crate::domain::ports::NewUserMessage;
use crate::domain::service::test_support::{
    build_message_service, ctx_for_subject, enforcer_allow, enforcer_allow_tenant_only,
    enforcer_allow_unconstrained, enforcer_deny, inmem_db, message_repo, seed_session,
};

fn harness_text_part(text: &str) -> MessagePartInput {
    MessagePartInput {
        part_type: chat_engine_sdk::models::MessagePartType::Text,
        content: serde_json::json!({ "text": text }),
        file_citations: vec![],
        link_citations: vec![],
        references: vec![],
    }
}

async fn harness_seed_pair(
    db: &std::sync::Arc<crate::infra::db::repo::ChatEngineDb>,
    session_id: Uuid,
    tenant: Uuid,
    user: Uuid,
) -> InsertedPair {
    message_repo(db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: None,
            parts: vec![harness_text_part("hi")],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed message pair")
}

#[tokio::test]
async fn resolve_owned_message_for_delete_pdp_denied_is_forbidden() {
    let db = inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let svc = build_message_service(&db, enforcer_deny());
    let err = svc
        .resolve_owned_message_for_delete(&ctx_for_subject(user, tenant), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn resolve_owned_message_for_delete_unknown_is_not_found() {
    let db = inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let svc = build_message_service(&db, enforcer_allow());
    let err = svc
        .resolve_owned_message_for_delete(&ctx_for_subject(user, tenant), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

#[tokio::test]
async fn resolve_owned_message_for_delete_owner_resolves() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = harness_seed_pair(&db, sid, tenant, user).await;

    let svc = build_message_service(&db, enforcer_allow());
    let msg = svc
        .resolve_owned_message_for_delete(&ctx_for_subject(user, tenant), pair.user_message_id)
        .await
        .expect("owner resolve ok");
    assert_eq!(msg.message_id, pair.user_message_id);
}

#[tokio::test]
async fn list_active_messages_pdp_denied_is_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_message_service(&db, enforcer_deny());
    let err = svc
        .list_active_messages(&ctx_for_subject(user, tenant), sid, None)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn list_active_messages_owner_returns_history() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    harness_seed_pair(&db, sid, tenant, user).await;

    let svc = build_message_service(&db, enforcer_allow());
    let msgs = svc
        .list_active_messages(&ctx_for_subject(user, tenant), sid, None)
        .await
        .expect("owner list ok");
    assert!(!msgs.is_empty(), "seeded active messages must be returned");
}

#[tokio::test]
async fn delete_message_cascade_pdp_denied_is_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_message_service(&db, enforcer_deny());
    let err = svc
        .delete_message_cascade(&ctx_for_subject(user, tenant), sid, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn delete_message_cascade_owner_removes_subtree() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    // Root pair (the session's first message cannot be deleted), then a child
    // reply under the root's assistant — the child subtree is deletable.
    let root = harness_seed_pair(&db, sid, tenant, user).await;
    let child = message_repo(&db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id: sid,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: Some(root.assistant_message_id),
            parts: vec![harness_text_part("follow up")],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed child pair");

    let svc = build_message_service(&db, enforcer_allow());
    let outcome = svc
        .delete_message_cascade(&ctx_for_subject(user, tenant), sid, child.user_message_id)
        .await
        .expect("owner delete ok");
    // Child + its assistant-stub descendant removed.
    assert_eq!(outcome.message_id, child.user_message_id);
    assert!(outcome.deleted_count >= 1, "at least the target is removed");
}

#[tokio::test]
async fn resolve_owned_message_read_owner_resolves() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = harness_seed_pair(&db, sid, tenant, user).await;

    let svc = build_message_service(&db, enforcer_allow());
    let msg = svc
        .resolve_owned_message(&ctx_for_subject(user, tenant), pair.user_message_id)
        .await
        .expect("owner read resolve ok");
    assert_eq!(msg.message_id, pair.user_message_id);
}

#[tokio::test]
async fn resolve_owned_message_read_pdp_denied_is_forbidden() {
    let db = inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let svc = build_message_service(&db, enforcer_deny());
    let err = svc
        .resolve_owned_message(&ctx_for_subject(user, tenant), Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn list_active_messages_with_parent_filter_returns_direct_replies() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let root = harness_seed_pair(&db, sid, tenant, user).await;
    // A reply under the root assistant, so the parent-filtered branch has data.
    message_repo(&db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id: sid,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: Some(root.assistant_message_id),
            parts: vec![harness_text_part("reply")],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed child");

    let svc = build_message_service(&db, enforcer_allow());
    let filtered = svc
        .list_active_messages(
            &ctx_for_subject(user, tenant),
            sid,
            Some(root.assistant_message_id),
        )
        .await
        .expect("parent-filtered list ok");
    // Every returned message is a direct child of the requested parent.
    assert!(
        filtered
            .iter()
            .all(|m| m.parent_message_id == Some(root.assistant_message_id))
    );
}

// ----------------- validate_message_metadata -----------------

#[test]
fn message_metadata_rejects_every_reserved_key() {
    for key in RESERVED_MESSAGE_METADATA_KEYS {
        let metadata = serde_json::json!({ *key: "spoofed" });
        let err =
            validate_message_metadata(Some(&metadata)).expect_err("reserved key must be rejected");
        assert!(
            matches!(err, ChatEngineError::BadRequest { .. }),
            "reserved key '{key}' must map to 400, got {err:?}",
        );
    }
}

#[test]
fn message_metadata_allows_client_keys_and_absent_metadata() {
    validate_message_metadata(None).expect("absent metadata accepted");
    let metadata = serde_json::json!({
        "device": "ios",
        "active_error": {"code": "E_OFFLINE"},
        "time_remaining_s": 42,
    });
    validate_message_metadata(Some(&metadata)).expect("client metadata accepted");
}

#[test]
fn message_metadata_enforces_the_serialized_size_cap() {
    // `{"blob":"…"}` — the payload sits just under / just over the cap once
    // the enclosing object and quotes are counted.
    let overhead = r#"{"blob":""}"#.len();
    let at_limit = serde_json::json!({ "blob": "x".repeat(MAX_MESSAGE_METADATA_BYTES - overhead) });
    validate_message_metadata(Some(&at_limit)).expect("payload at the cap is accepted");

    let over_limit =
        serde_json::json!({ "blob": "x".repeat(MAX_MESSAGE_METADATA_BYTES - overhead + 1) });
    let err = validate_message_metadata(Some(&over_limit))
        .expect_err("payload over the cap must be rejected");
    assert!(
        matches!(err, ChatEngineError::BadRequest { .. }),
        "oversized metadata must map to 400, got {err:?}",
    );
}

#[test]
fn message_metadata_accepts_non_object_json() {
    // Reserved-key filtering only applies to objects; a scalar or array is
    // opaque client context and only the size cap governs it.
    validate_message_metadata(Some(&serde_json::json!(["a", "b"]))).expect("array accepted");
    validate_message_metadata(Some(&serde_json::json!("plain"))).expect("string accepted");
}

// ===========================================================================
// Ownership guard: the PDP scopes the tenant, the gear scopes the owner.
// `enforcer_allow_tenant_only` models the shipped policy plugins, neither of
// which emits an `owner_id` predicate.
// @cpt-cf-chat-engine-nfr-authentication
// ===========================================================================

/// Point-op read of a same-tenant stranger's message must 404, not resolve.
#[tokio::test]
async fn get_message_same_tenant_stranger_is_not_found_under_tenant_only_pdp() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    let pair = harness_seed_pair(&db, sid, tenant, owner).await;

    let svc = build_message_service(&db, enforcer_allow_tenant_only());
    let err = svc
        .resolve_owned_message(
            &ctx_for_subject(Uuid::new_v4(), tenant),
            pair.user_message_id,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "a same-tenant stranger must not read another user's message, got: {err:?}",
    );

    // Control: the owner still reads it under the same PDP.
    svc.resolve_owned_message(&ctx_for_subject(owner, tenant), pair.user_message_id)
        .await
        .expect("owner reads its own message");
}

/// The clamp rides into the `WHERE` clause, so a stranger's list is empty
/// rather than a page of someone else's conversation.
#[tokio::test]
async fn list_active_messages_hides_other_users_rows_under_tenant_only_pdp() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    harness_seed_pair(&db, sid, tenant, owner).await;

    let svc = build_message_service(&db, enforcer_allow_tenant_only());
    let seen = svc
        .list_active_messages(&ctx_for_subject(Uuid::new_v4(), tenant), sid, None)
        .await
        .expect("list is authorized, it is the rows that are scoped away");
    assert!(
        seen.is_empty(),
        "a same-tenant stranger must not list another user's messages, got {} rows",
        seen.len(),
    );

    let owned = svc
        .list_active_messages(&ctx_for_subject(owner, tenant), sid, None)
        .await
        .expect("owner lists its own messages");
    assert!(!owned.is_empty(), "the owner's own page must be intact");
}

/// Deletes are clamped the same way, and the row survives the attempt.
#[tokio::test]
async fn delete_message_by_same_tenant_stranger_is_not_found_under_tenant_only_pdp() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    let root = harness_seed_pair(&db, sid, tenant, owner).await;
    let child = message_repo(&db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id: sid,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(owner.to_string()),
            parent_message_id: Some(root.assistant_message_id),
            parts: vec![harness_text_part("child")],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed child pair");

    let svc = build_message_service(&db, enforcer_allow_tenant_only());
    let err = svc
        .delete_message_cascade(
            &ctx_for_subject(Uuid::new_v4(), tenant),
            sid,
            child.user_message_id,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    svc.resolve_owned_message(&ctx_for_subject(owner, tenant), child.user_message_id)
        .await
        .expect("the message must survive a stranger's delete");
}

/// Unlike the session point-ops, MESSAGE point-ops ask the PDP with
/// `require_constraints = true`, so an allow carrying no constraints cannot
/// compile a scope and fails closed before any row is touched. Pinned here so
/// the fail-closed half of the contract does not silently become an
/// `allow_all` fast path.
#[tokio::test]
async fn get_message_unconstrained_allow_fails_closed() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    let pair = harness_seed_pair(&db, sid, tenant, owner).await;

    let svc = build_message_service(&db, enforcer_allow_unconstrained());
    let err = svc
        .resolve_owned_message(&ctx_for_subject(owner, tenant), pair.user_message_id)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "an allow with no constraints must fail closed on a MESSAGE point-op, got: {err:?}",
    );
}
