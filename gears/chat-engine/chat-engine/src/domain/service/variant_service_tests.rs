use super::*;

use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// ------------------------------------------------------------------
//  Mock VariantRepo that simulates UNIQUE violations.
// ------------------------------------------------------------------

/// Repository double that emulates the variant_index allocator's
/// race-and-retry behaviour. Configurable: each call to
/// `insert_user_and_assistant_stub_for_branch` fails up to
/// `fail_until_attempt` times before succeeding (mimics the
/// SERIALIZABLE retry loop's UNIQUE constraint contention).
struct RetryingMockRepo {
    /// Counts attempts per `(session_id, parent_message_id)` key.
    attempts: Mutex<std::collections::HashMap<(Uuid, Uuid), usize>>,
    /// Force every call to error out (simulates exhausted retries).
    always_fail: bool,
    /// On the Nth attempt (0-based) succeed; earlier attempts
    /// surface a UNIQUE-violation-like error.
    succeed_on_attempt: usize,
    /// Number of times the combined insert has been called across
    /// all keys.
    total_calls: AtomicUsize,
}

impl RetryingMockRepo {
    fn new(succeed_on_attempt: usize) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(std::collections::HashMap::new()),
            always_fail: false,
            succeed_on_attempt,
            total_calls: AtomicUsize::new(0),
        })
    }

    fn always_failing() -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(std::collections::HashMap::new()),
            always_fail: true,
            succeed_on_attempt: usize::MAX,
            total_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl VariantRepo for RetryingMockRepo {
    async fn list_siblings(
        &self,
        _session_id: Uuid,
        _parent_message_id: Option<Uuid>,
    ) -> Result<Vec<Message>> {
        Ok(Vec::new())
    }

    async fn insert_user_and_assistant_stub_for_branch(
        &self,
        session_id: Uuid,
        parent_message_id: Uuid,
        _parts: Vec<MessagePartInput>,
        _file_ids: Option<Vec<Uuid>>,
        _tenant_id: Option<String>,
        _user_id: Option<String>,
    ) -> Result<(Uuid, i32, Uuid)> {
        self.total_calls.fetch_add(1, Ordering::SeqCst);
        let mut attempts = self.attempts.lock();
        let key = (session_id, parent_message_id);
        let n = attempts.entry(key).or_insert(0);
        let attempt_idx = *n;
        *n += 1;
        drop(attempts);

        if self.always_fail {
            return Err(ChatEngineError::internal(
                "assign_variant_index exhausted 3 retries (uq_messages_session_parent_variant)",
            ));
        }
        if attempt_idx < self.succeed_on_attempt {
            return Err(ChatEngineError::internal(
                "UNIQUE constraint violated: uq_messages_session_parent_variant",
            ));
        }
        Ok((
            Uuid::new_v4(),
            i32::try_from(attempt_idx).unwrap_or(0),
            Uuid::new_v4(),
        ))
    }

    async fn ancestor_chain(&self, _session_id: Uuid, message_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(vec![message_id])
    }

    async fn collect_descendants(&self, _session_id: Uuid, _message_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(Vec::new())
    }

    async fn apply_active_flips(
        &self,
        _session_id: Uuid,
        _activate_ids: Vec<Uuid>,
        _deactivate_ids: Vec<Uuid>,
    ) -> Result<()> {
        Ok(())
    }

    async fn update_session_type(
        &self,
        _tenant_id: &str,
        _user_id: &str,
        _session_id: Uuid,
        _new_session_type_id: Uuid,
        _new_capabilities: JsonValue,
    ) -> Result<Session> {
        Err(ChatEngineError::internal("not implemented for this mock"))
    }
}

// ------------------------------------------------------------------
//  Race-with-retry: a helper that emulates the SERIALIZABLE retry
//  loop semantics directly against the mock repo.
// ------------------------------------------------------------------

/// Drive `insert_user_and_assistant_stub_for_branch` with up to
/// `max_attempts` retries on a UNIQUE-violation-like `Internal`
/// error, then promote a final exhaustion into a `Conflict`.
/// Mirrors the production retry semantics enforced by
/// `assign_variant_index` + the `map_unique_violation_to_conflict`
/// wrapper.
async fn insert_with_retry(
    repo: Arc<dyn VariantRepo>,
    session_id: Uuid,
    parent_message_id: Uuid,
    max_attempts: usize,
) -> Result<(Uuid, i32, Uuid)> {
    let mut last_err: Option<ChatEngineError> = None;
    for _ in 0..max_attempts {
        match repo
            .insert_user_and_assistant_stub_for_branch(
                session_id,
                parent_message_id,
                vec![MessagePartInput {
                    part_type: chat_engine_sdk::models::MessagePartType::Text,
                    content: json!({"text": "x"}),
                    file_citations: vec![],
                    link_citations: vec![],
                    references: vec![],
                }],
                None,
                None,
                None,
            )
            .await
        {
            Ok(v) => return Ok(v),
            Err(err) => {
                let is_retryable = matches!(
                    &err,
                    ChatEngineError::Internal { reason, .. }
                        if reason.to_lowercase().contains("unique")
                            || reason.to_lowercase().contains("exhausted")
                );
                if !is_retryable {
                    return Err(err);
                }
                last_err = Some(err);
            }
        }
    }
    Err(map_unique_violation_to_conflict(last_err.unwrap_or_else(
        || ChatEngineError::internal("exhausted retries with no recorded error"),
    )))
}

#[tokio::test]
async fn assign_variant_index_race_retries_then_succeeds() {
    // Succeed on the 3rd attempt (0-indexed: succeed_on_attempt=2).
    // Capped at the same 3 attempts the production helper uses.
    let repo: Arc<dyn VariantRepo> = RetryingMockRepo::new(2);
    let session_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let (_user_id, variant_index, _assistant_id) =
        insert_with_retry(Arc::clone(&repo), session_id, parent, 3)
            .await
            .expect("should succeed within 3 retries");
    assert_eq!(
        variant_index, 2,
        "should reflect the attempt that succeeded"
    );
}

#[tokio::test]
async fn assign_variant_index_exhausted_retries_returns_conflict() {
    let repo: Arc<dyn VariantRepo> = RetryingMockRepo::always_failing();
    let session_id = Uuid::new_v4();
    let parent = Uuid::new_v4();
    let err = insert_with_retry(Arc::clone(&repo), session_id, parent, 3)
        .await
        .expect_err("must exhaust retries");
    assert!(
        matches!(err, ChatEngineError::Conflict { .. }),
        "expected Conflict, got {err:?}"
    );
}

#[test]
fn augment_complete_event_merges_variant_info_into_existing_metadata() {
    use crate::domain::message::StreamingCompleteEvent;

    let info = json!({
        "message_id": "00000000-0000-0000-0000-000000000001",
        "variant_index": 2,
        "total_variants": 3,
        "is_active": true,
    });
    let evt = StreamingEvent::Complete(StreamingCompleteEvent {
        message_id: Uuid::nil(),
        metadata: Some(json!({"model": "gpt-test"})),
        file_citations: vec![],
        link_citations: vec![],
        references: vec![],
    });
    let out = augment_complete_event(evt, &info);
    match out {
        StreamingEvent::Complete(c) => {
            let meta = c.metadata.expect("metadata must be present");
            assert_eq!(meta["model"], "gpt-test");
            assert_eq!(meta["variant_info"]["variant_index"], 2);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn augment_complete_event_creates_metadata_when_absent() {
    use crate::domain::message::StreamingCompleteEvent;

    let info = json!({"variant_index": 0});
    let evt = StreamingEvent::Complete(StreamingCompleteEvent {
        message_id: Uuid::nil(),
        metadata: None,
        file_citations: vec![],
        link_citations: vec![],
        references: vec![],
    });
    let out = augment_complete_event(evt, &info);
    match out {
        StreamingEvent::Complete(c) => {
            let meta = c.metadata.expect("metadata must be created");
            assert_eq!(meta["variant_info"]["variant_index"], 0);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn map_unique_violation_to_conflict_only_converts_known_messages() {
    let benign = ChatEngineError::internal("totally unrelated db hiccup");
    match map_unique_violation_to_conflict(benign) {
        ChatEngineError::Internal { .. } => {}
        other => panic!("benign internal must stay Internal, got {other:?}"),
    }
    let exhausted = ChatEngineError::internal(
        "assign_variant_index exhausted 3 retries (uq_messages_session_parent_variant)",
    );
    match map_unique_violation_to_conflict(exhausted) {
        ChatEngineError::Conflict { .. } => {}
        other => panic!("exhausted internal must map to Conflict, got {other:?}"),
    }
}

#[test]
fn enabled_capability_names_handles_missing_and_malformed_inputs() {
    use chat_engine_sdk::models::{TenantId, UserId};
    let mut s = Session {
        session_id: Uuid::nil(),
        tenant_id: TenantId::new("t"),
        user_id: UserId::new("u"),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata: None,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };
    assert!(enabled_capability_names(&s).is_empty());

    s.enabled_capabilities =
        Some(json!([{"name": "model", "value": "x"}, {"name": "stream", "value": true}]));
    let names = enabled_capability_names(&s);
    assert_eq!(names, vec!["model".to_string(), "stream".to_string()]);

    s.enabled_capabilities = Some(json!("not an array"));
    assert!(enabled_capability_names(&s).is_empty());
}

// ===========================================================================
// Authorization + happy-path suite: real Sea-ORM repos over in-memory SQLite
// with a mock PDP, so the VariantService PEP flow (authorize_session_op) and
// the read path run end-to-end.
// ===========================================================================

use crate::domain::ports::NewUserMessage;
use crate::domain::service::test_support::{
    build_variant_service, ctx_for_subject, enforcer_allow, enforcer_deny, inmem_db, message_repo,
    seed_session,
};
use chat_engine_sdk::models::{MessagePartInput, MessagePartType};

fn text_part(text: &str) -> MessagePartInput {
    MessagePartInput {
        part_type: MessagePartType::Text,
        content: json!({ "text": text }),
        file_citations: vec![],
        link_citations: vec![],
        references: vec![],
    }
}

async fn seed_message_pair(
    db: &std::sync::Arc<crate::infra::db::repo::ChatEngineDb>,
    session_id: Uuid,
    tenant: Uuid,
    user: Uuid,
) -> crate::domain::ports::InsertedPair {
    message_repo(db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: None,
            parts: vec![text_part("hi")],
            file_ids: None,
            metadata: None,
        })
        .await
        .expect("seed message pair")
}

#[tokio::test]
async fn list_variants_pdp_denied_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_deny());
    let err = svc
        .list_variants(&ctx_for_subject(user, tenant), sid, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn set_active_variant_pdp_denied_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_deny());
    let err = svc
        .set_active_variant(&ctx_for_subject(user, tenant), sid, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn set_active_variant_by_index_pdp_denied_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_deny());
    let err = svc
        .set_active_variant_by_index(&ctx_for_subject(user, tenant), sid, Uuid::new_v4(), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn list_variants_owner_returns_sibling_set() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = seed_message_pair(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_allow());
    let listing = svc
        .list_variants(
            &ctx_for_subject(user, tenant),
            sid,
            pair.assistant_message_id,
        )
        .await
        .expect("owner list_variants ok");
    assert!(
        !listing.variants.is_empty(),
        "the assistant stub is its own single variant"
    );
}

#[tokio::test]
async fn list_variants_unknown_message_is_not_found() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_allow());
    let err = svc
        .list_variants(&ctx_for_subject(user, tenant), sid, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

#[tokio::test]
async fn set_active_variant_owner_activates_message() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = seed_message_pair(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_allow());
    let entry = svc
        .set_active_variant(
            &ctx_for_subject(user, tenant),
            sid,
            pair.assistant_message_id,
        )
        .await
        .expect("owner set_active_variant ok");
    assert_eq!(entry.message.message_id, pair.assistant_message_id);
    assert!(entry.info.is_active);
}

#[tokio::test]
async fn set_active_variant_by_index_owner_activates_sibling() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = seed_message_pair(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_allow());
    // The assistant stub is the sole sibling under the user message at index 0.
    let entry = svc
        .set_active_variant_by_index(
            &ctx_for_subject(user, tenant),
            sid,
            pair.assistant_message_id,
            0,
        )
        .await
        .expect("owner set_active_variant_by_index ok");
    assert_eq!(entry.info.variant_index, 0);
}

#[tokio::test]
async fn set_active_variant_by_index_wrong_tenant_returns_not_found() {
    // Point-op scope-miss -> 404 (anti-enumeration, ADR-0021). The session is
    // owned by tenant_a/user_a; a foreign subject gets a PDP allow constrained
    // to its OWN owner pair, so `authorize_session_op` re-reads under that scope
    // and resolves 0 rows. This is only correct because the point-op applies the
    // compiled scope to the row instead of consuming the unrestricted prefetch —
    // a regression would leak the cross-tenant session as an activatable target.
    let db = inmem_db().await;
    let (tenant_a, user_a, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant_a, user_a).await;
    let pair = seed_message_pair(&db, sid, tenant_a, user_a).await;

    let svc = build_variant_service(&db, enforcer_allow());
    let err = svc
        .set_active_variant_by_index(
            &ctx_for_subject(Uuid::new_v4(), Uuid::new_v4()),
            sid,
            pair.assistant_message_id,
            0,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "Expected NotFound for cross-tenant variant activation, got: {err:?}"
    );
}

#[tokio::test]
async fn set_active_variant_by_index_unknown_index_is_not_found() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let pair = seed_message_pair(&db, sid, tenant, user).await;

    let svc = build_variant_service(&db, enforcer_allow());
    let err = svc
        .set_active_variant_by_index(
            &ctx_for_subject(user, tenant),
            sid,
            pair.assistant_message_id,
            99,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

// ===========================================================================
// switch_session_type end to end.
//
// This surface had no test at all, which is how a broken owner predicate in
// `variant_repo::update_session_type` shipped: it compared the UUID-typed
// `sessions.tenant_id` / `user_id` columns against the caller's id STRINGS,
// matched zero rows, and returned `NotFound` — a 404 on
// `POST /sessions/{id}/switch-type` for a session the caller owns.
// @cpt-cf-chat-engine-fr-switch-session-type
// ===========================================================================

struct SwitchPlugin {
    id: String,
    capabilities: Vec<chat_engine_sdk::models::Capability>,
    metadata: Option<JsonValue>,
}

impl SwitchPlugin {
    fn new(id: &str, capability_names: &[&str], metadata: Option<JsonValue>) -> Arc<Self> {
        Arc::new(Self {
            id: id.to_owned(),
            capabilities: capability_names
                .iter()
                .map(|name| chat_engine_sdk::models::Capability {
                    name: (*name).to_owned(),
                    value: serde_json::json!({ "type": "bool", "default_value": true }),
                })
                .collect(),
            metadata,
        })
    }
}

#[async_trait::async_trait]
impl chat_engine_sdk::ChatEngineBackendPlugin for SwitchPlugin {
    fn plugin_instance_id(&self) -> &str {
        &self.id
    }

    async fn on_session_updated(
        &self,
        _ctx: chat_engine_sdk::plugin::SessionPluginCtx,
    ) -> std::result::Result<
        chat_engine_sdk::plugin::SessionPluginResponse,
        chat_engine_sdk::PluginError,
    > {
        Ok(chat_engine_sdk::plugin::SessionPluginResponse {
            capabilities: self.capabilities.clone(),
            metadata: self.metadata.clone(),
        })
    }
}

/// Build a `VariantService` whose plugin hub actually carries `plugin`, so the
/// switch reaches the session write instead of stopping at plugin resolution.
fn variant_service_with_plugin(
    db: &Arc<crate::infra::db::repo::ChatEngineDb>,
    plugin_instance_id: &str,
    plugin: Arc<dyn chat_engine_sdk::ChatEngineBackendPlugin>,
    enforcer: PolicyEnforcer,
) -> VariantService {
    use crate::domain::service::test_support::{
        message_repo, session_repo, session_type_repo, variant_repo,
    };
    use crate::infra::db::repo::plugin_config_repo::SeaPluginConfigRepo;

    let hub = Arc::new(toolkit::ClientHub::new());
    hub.register_scoped::<dyn chat_engine_sdk::ChatEngineBackendPlugin>(
        toolkit::client_hub::ClientScope::gts_id(plugin_instance_id),
        plugin,
    );
    let plugins = PluginService::new(hub, Arc::new(SeaPluginConfigRepo::new(Arc::clone(db))));
    let message_service = Arc::new(MessageService::new(
        session_repo(db),
        session_type_repo(db),
        message_repo(db),
        plugins.clone(),
        enforcer.clone(),
    ));

    VariantService::new(
        session_repo(db),
        session_type_repo(db),
        message_repo(db),
        variant_repo(db),
        plugins,
        message_service,
        enforcer,
    )
}

async fn seed_target_type(
    db: &Arc<crate::infra::db::repo::ChatEngineDb>,
    plugin_instance_id: &str,
) -> Uuid {
    use crate::domain::ports::NewSessionType;
    use crate::domain::service::test_support::session_type_repo;

    let now = OffsetDateTime::now_utc();
    session_type_repo(db)
        .insert(NewSessionType {
            session_type_id: Uuid::new_v4(),
            name: "target-type".to_owned(),
            plugin_instance_id: Some(plugin_instance_id.to_owned()),
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("seed target session type")
        .session_type_id
}

/// The owner switching an existing session's type gets the updated session,
/// not a 404.
#[tokio::test]
async fn switch_session_type_owner_updates_the_session() {
    const PLUGIN: &str = "gts.test.switch_owner.v1~";

    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let target = seed_target_type(&db, PLUGIN).await;

    let svc = variant_service_with_plugin(
        &db,
        PLUGIN,
        SwitchPlugin::new(PLUGIN, &["feedback"], None),
        enforcer_allow(),
    );

    let updated = svc
        .switch_session_type(&ctx_for_subject(user, tenant), sid, target)
        .await
        .expect("the owner must be able to switch its own session's type");
    assert_eq!(updated.session_id, sid);
    assert_eq!(updated.session_type_id, Some(target));
    assert!(
        updated.enabled_capabilities.is_some(),
        "the plugin's capability list must be persisted on the session",
    );
}

/// Plugin-returned metadata is merged into the session on a successful switch —
/// the second legacy owner-filtered write on this path.
#[tokio::test]
async fn switch_session_type_merges_plugin_metadata() {
    const PLUGIN: &str = "gts.test.switch_meta.v1~";

    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let target = seed_target_type(&db, PLUGIN).await;

    let svc = variant_service_with_plugin(
        &db,
        PLUGIN,
        SwitchPlugin::new(
            PLUGIN,
            &["feedback"],
            Some(serde_json::json!({ "greeting": "hi" })),
        ),
        enforcer_allow(),
    );

    let updated = svc
        .switch_session_type(&ctx_for_subject(user, tenant), sid, target)
        .await
        .expect("switch with metadata merge");
    assert_eq!(
        updated
            .metadata
            .as_ref()
            .and_then(|m| m.get("greeting"))
            .and_then(serde_json::Value::as_str),
        Some("hi"),
        "plugin metadata must land on the session",
    );
}

/// A stranger in the same tenant must still be refused — the owner guard runs
/// before the PDP decision, so this is 404 regardless of policy.
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn switch_session_type_same_tenant_stranger_is_not_found() {
    const PLUGIN: &str = "gts.test.switch_stranger.v1~";

    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    let target = seed_target_type(&db, PLUGIN).await;

    let svc = variant_service_with_plugin(
        &db,
        PLUGIN,
        SwitchPlugin::new(PLUGIN, &["feedback"], None),
        crate::domain::service::test_support::enforcer_allow_tenant_only(),
    );

    let err = svc
        .switch_session_type(&ctx_for_subject(Uuid::new_v4(), tenant), sid, target)
        .await
        .expect_err("a same-tenant stranger must not switch another user's session type");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");
}
