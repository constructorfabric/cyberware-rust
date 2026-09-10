use super::*;
use crate::domain::ports::NewSession;
use crate::domain::ports::NewSessionType;
use crate::domain::session::Session;
use crate::domain::session::SessionType;
use async_trait::async_trait;
use chat_engine_sdk::models::LifecycleState;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use time::OffsetDateTime;
use toolkit::ClientHub;
use uuid::Uuid;

use crate::domain::message::{Message, MessagePart, MessageRole};
use crate::domain::ports::PluginConfigRepo;
use crate::domain::ports::SessionRepo;
use crate::domain::ports::SessionTypeRepo;
use crate::domain::ports::{FinalizeOutcome, InsertedPair, MessageRepo, NewUserMessage};
use crate::domain::ports::{ReactionDeleteOutcome, ReactionRepo, ReactionUpsertOutcome};
use crate::domain::service::test_support;

// ----------------------------- Stubs ----------------------------------

struct StubSessionRepo {
    session: Mutex<Session>,
}

impl StubSessionRepo {
    fn new(session: Session) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(session),
        })
    }
}

#[async_trait]
impl SessionRepo for StubSessionRepo {
    async fn insert(&self, _m: NewSession) -> std::result::Result<Session, ChatEngineError> {
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
        _i: Uuid,
        _m: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn update_capabilities(
        &self,
        _t: &str,
        _u: &str,
        _i: Uuid,
        _c: Option<JsonValue>,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn update_lifecycle_state(
        &self,
        _t: &str,
        _u: &str,
        _i: Uuid,
        _s: LifecycleState,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn soft_delete(
        &self,
        _t: &str,
        _u: &str,
        _i: Uuid,
        _d: i64,
    ) -> std::result::Result<Session, ChatEngineError> {
        Ok(self.session.lock().clone())
    }

    async fn hard_delete(
        &self,
        _t: &str,
        _u: &str,
        _i: Uuid,
    ) -> std::result::Result<bool, ChatEngineError> {
        Ok(true)
    }

    // Scoped (Phase 4) prefetch used by authorize_session under a permissive
    // enforcer: ignore the scope, return the fixed session if the id matches.
    async fn find_by_id_scoped(
        &self,
        _scope: &AccessScope,
        session_id: Uuid,
    ) -> std::result::Result<Option<Session>, ChatEngineError> {
        let s = self.session.lock().clone();
        Ok((s.session_id == session_id).then_some(s))
    }
}

struct StubSessionTypeRepo;

#[async_trait]
impl SessionTypeRepo for StubSessionTypeRepo {
    async fn insert(
        &self,
        _m: NewSessionType,
    ) -> std::result::Result<SessionType, ChatEngineError> {
        unreachable!()
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

struct StubMessageRepo {
    message: Mutex<Option<Message>>,
}

impl StubMessageRepo {
    fn assistant(session_id: Uuid, message_id: Uuid) -> Arc<Self> {
        let now = OffsetDateTime::now_utc();
        let msg = Message {
            message_id,
            session_id,
            tenant_id: None,
            user_id: None,
            parent_message_id: None,
            variant_index: 0,
            is_active: true,
            role: MessageRole::Assistant,
            parts: vec![MessagePart::text(Uuid::nil(), Uuid::nil(), 0, "hi")],
            file_ids: vec![],
            metadata: None,
            is_complete: true,
            is_hidden_from_user: false,
            is_hidden_from_backend: false,
            created_at: now,
            updated_at: now,
        };
        Arc::new(Self {
            message: Mutex::new(Some(msg)),
        })
    }

    fn user(session_id: Uuid, message_id: Uuid) -> Arc<Self> {
        let now = OffsetDateTime::now_utc();
        let msg = Message {
            message_id,
            session_id,
            tenant_id: None,
            user_id: None,
            parent_message_id: None,
            variant_index: 0,
            is_active: true,
            role: MessageRole::User,
            parts: vec![MessagePart::text(Uuid::nil(), Uuid::nil(), 0, "hi")],
            file_ids: vec![],
            metadata: None,
            is_complete: true,
            is_hidden_from_user: false,
            is_hidden_from_backend: false,
            created_at: now,
            updated_at: now,
        };
        Arc::new(Self {
            message: Mutex::new(Some(msg)),
        })
    }
}

#[async_trait]
impl MessageRepo for StubMessageRepo {
    async fn insert_user_and_assistant_stub(
        &self,
        _req: NewUserMessage,
    ) -> std::result::Result<InsertedPair, ChatEngineError> {
        unreachable!()
    }

    async fn finalize_assistant(
        &self,
        _session_id: Uuid,
        _id: Uuid,
        _outcome: FinalizeOutcome,
    ) -> std::result::Result<(), ChatEngineError> {
        unreachable!()
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
        let m = self.message.lock().clone();
        Ok(m.filter(|msg| msg.session_id == session_id && msg.message_id == message_id))
    }
}

#[derive(Default)]
struct StubReactionRepo {
    upsert_calls: AtomicUsize,
    delete_calls: AtomicUsize,
    list_returns: Mutex<Vec<MessageReaction>>,
}

#[async_trait]
impl ReactionRepo for StubReactionRepo {
    async fn get_by_pk(
        &self,
        _message_id: Uuid,
        _user_id: &str,
    ) -> std::result::Result<Option<MessageReaction>, ChatEngineError> {
        Ok(None)
    }

    async fn upsert(
        &self,
        message_id: Uuid,
        user_id: &str,
        reaction_type: ReactionType,
    ) -> std::result::Result<ReactionUpsertOutcome, ChatEngineError> {
        self.upsert_calls.fetch_add(1, Ordering::SeqCst);
        let now = OffsetDateTime::now_utc();
        Ok(ReactionUpsertOutcome {
            reaction: MessageReaction {
                message_id,
                user_id: user_id.to_owned(),
                reaction_type,
                created_at: now,
                updated_at: now,
            },
            previous_reaction_type: None,
        })
    }

    async fn delete(
        &self,
        _message_id: Uuid,
        _user_id: &str,
    ) -> std::result::Result<ReactionDeleteOutcome, ChatEngineError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ReactionDeleteOutcome {
            applied: true,
            previous_reaction_type: Some(ReactionType::Like),
        })
    }

    async fn list_by_message(
        &self,
        _message_id: Uuid,
    ) -> std::result::Result<Vec<MessageReaction>, ChatEngineError> {
        Ok(self.list_returns.lock().clone())
    }

    // Batch (trust-parent) read used by list_for_messages: return the seeded
    // reactions whose message_id is in the requested set.
    async fn list_by_messages(
        &self,
        message_ids: &[Uuid],
    ) -> std::result::Result<Vec<MessageReaction>, ChatEngineError> {
        Ok(self
            .list_returns
            .lock()
            .iter()
            .filter(|r| message_ids.contains(&r.message_id))
            .cloned()
            .collect())
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

/// Owner pair carried by every fixture session in this module; [`make_ctx`]
/// builds a context for the same pair, so the caller is the session owner —
/// what `owner_guard::ensure_session_owner` requires of an authorized op.
const OWNER_TENANT: Uuid = Uuid::from_u128(0x0A11);
const OWNER_USER: Uuid = Uuid::from_u128(0x0B22);

fn make_session(session_id: Uuid, enabled_capabilities: Option<JsonValue>) -> Session {
    let now = OffsetDateTime::now_utc();
    Session {
        session_id,
        tenant_id: OWNER_TENANT.to_string().into(),
        user_id: OWNER_USER.to_string().into(),
        client_id: None,
        session_type_id: None,
        enabled_capabilities,
        metadata: None,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: now,
        updated_at: now,
    }
}

fn plugin_service() -> PluginService {
    PluginService::new(Arc::new(ClientHub::new()), Arc::new(StubPluginConfigRepo))
}

fn make_service(
    sessions: Arc<dyn SessionRepo>,
    messages: Arc<dyn MessageRepo>,
    reactions: Arc<dyn ReactionRepo>,
) -> ReactionService {
    make_service_with_enforcer(
        sessions,
        messages,
        reactions,
        test_support::enforcer_allow(),
    )
}

fn make_service_with_enforcer(
    sessions: Arc<dyn SessionRepo>,
    messages: Arc<dyn MessageRepo>,
    reactions: Arc<dyn ReactionRepo>,
    enforcer: PolicyEnforcer,
) -> ReactionService {
    ReactionService::new(
        sessions,
        Arc::new(StubSessionTypeRepo),
        messages,
        reactions,
        plugin_service(),
        enforcer,
    )
}

fn make_ctx() -> SecurityContext {
    test_support::ctx_for_subject(OWNER_USER, OWNER_TENANT)
}

// --------------------------- Unit tests -------------------------------

#[tokio::test]
async fn set_reaction_returns_409_when_feedback_capability_missing() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "model", "value": "gpt-4" }])),
    );
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
    );

    let err = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::Like)
        .await
        .expect_err("capability gate must reject");
    match err {
        ChatEngineError::Conflict { reason } => {
            assert!(reason.contains("feedback"), "reason mentions capability");
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn set_reaction_upserts_when_capability_enabled() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let reactions = Arc::new(StubReactionRepo::default());
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        reactions.clone(),
    );

    let (resp, mutation) = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::Like)
        .await
        .expect("ok");
    assert_eq!(resp.message_id, message_id);
    assert_eq!(resp.reaction_type, ReactionType::Like);
    assert!(resp.applied);
    assert_eq!(reactions.upsert_calls.load(Ordering::SeqCst), 1);
    assert_eq!(mutation.reaction_type, ReactionType::Like);
}

#[tokio::test]
async fn set_reaction_deletes_on_none_with_applied_true() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let reactions = Arc::new(StubReactionRepo::default());
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        reactions.clone(),
    );

    let (resp, mutation) = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::None)
        .await
        .expect("ok");
    assert_eq!(resp.reaction_type, ReactionType::None);
    assert!(resp.applied);
    assert_eq!(reactions.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(mutation.previous_reaction_type, Some(ReactionType::Like));
}

#[tokio::test]
async fn set_reaction_returns_404_on_unknown_session() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
    );

    // authorize_session prefetches via `find_by_id_scoped`; an id the repo
    // does not hold resolves to None and collapses to a 404.
    let err = svc
        .set_reaction(&make_ctx(), Uuid::new_v4(), message_id, ReactionType::Like)
        .await
        .expect_err("missing session must be 404");
    assert!(matches!(
        err,
        ChatEngineError::NotFound {
            resource: "session",
            ..
        }
    ));
}

#[tokio::test]
async fn set_reaction_returns_400_on_non_assistant_target() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::user(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
    );

    let err = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::Like)
        .await
        .expect_err("user-message target must be rejected");
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn list_reactions_bypasses_capability_gate() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    // No feedback capability — the read path must still succeed.
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "model", "value": "gpt-4" }])),
    );
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
    );

    let listing = svc
        .list_reactions(&make_ctx(), session_id, message_id)
        .await
        .expect("ok");
    assert_eq!(listing.message_id, message_id);
    assert!(listing.reactions.is_empty());
}

#[tokio::test]
async fn list_for_messages_groups_reactions_by_message_id() {
    let session_id = Uuid::new_v4();
    let m1 = Uuid::new_v4();
    let m2 = Uuid::new_v4();
    let m3 = Uuid::new_v4(); // requested but has no reactions
    let now = OffsetDateTime::now_utc();
    let seeded = vec![
        MessageReaction {
            message_id: m1,
            user_id: "u".into(),
            reaction_type: ReactionType::Like,
            created_at: now,
            updated_at: now,
        },
        MessageReaction {
            message_id: m1,
            user_id: "v".into(),
            reaction_type: ReactionType::Dislike,
            created_at: now,
            updated_at: now,
        },
        MessageReaction {
            message_id: m2,
            user_id: "u".into(),
            reaction_type: ReactionType::Like,
            created_at: now,
            updated_at: now,
        },
    ];
    let repo = Arc::new(StubReactionRepo {
        list_returns: Mutex::new(seeded),
        ..Default::default()
    });
    let session = make_session(session_id, None);
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, m1),
        repo,
    );

    let map = svc
        .list_for_messages(&make_ctx(), &[m1, m2, m3])
        .await
        .expect("ok");

    assert_eq!(map.get(&m1).map(Vec::len), Some(2));
    assert_eq!(map.get(&m2).map(Vec::len), Some(1));
    assert!(
        !map.contains_key(&m3),
        "messages with no reactions are absent"
    );
}

#[tokio::test]
async fn list_for_messages_empty_input_short_circuits() {
    let session = make_session(Uuid::new_v4(), None);
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(Uuid::new_v4(), Uuid::new_v4()),
        Arc::new(StubReactionRepo::default()),
    );

    let map = svc.list_for_messages(&make_ctx(), &[]).await.expect("ok");
    assert!(map.is_empty());
}

#[tokio::test]
async fn list_reactions_unknown_message_returns_empty() {
    let session_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let svc = make_service(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, Uuid::new_v4()),
        Arc::new(StubReactionRepo::default()),
    );

    // The scoped read path no longer probes message existence: it lists the
    // caller's owned reactions and returns an empty set (anti-enumeration)
    // rather than leaking a 404 for an unknown/foreign message id.
    let unknown_message = Uuid::new_v4();
    let listing = svc
        .list_reactions(&make_ctx(), session_id, unknown_message)
        .await
        .expect("unknown message lists empty, not 404");
    assert_eq!(listing.message_id, unknown_message);
    assert!(listing.reactions.is_empty());
}

#[tokio::test]
async fn list_reactions_excludes_reactions_for_mismatched_session() {
    // A reaction exists for `message_id`, but the message belongs to a
    // DIFFERENT session than the one being listed. The pair is invalid, so the
    // listing is empty — the reaction must not leak across a mismatched
    // (session_id, message_id) pair inside an otherwise authorized session.
    let authorized_session = Uuid::new_v4();
    let other_session = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();

    let reactions = Arc::new(StubReactionRepo {
        list_returns: Mutex::new(vec![MessageReaction {
            message_id,
            user_id: "u".into(),
            reaction_type: ReactionType::Like,
            created_at: now,
            updated_at: now,
        }]),
        ..Default::default()
    });
    let svc = make_service(
        StubSessionRepo::new(make_session(authorized_session, None)),
        // The message lives in the other session only.
        StubMessageRepo::assistant(other_session, message_id),
        reactions,
    );

    let listing = svc
        .list_reactions(&make_ctx(), authorized_session, message_id)
        .await
        .expect("mismatched pair lists empty");
    assert_eq!(listing.message_id, message_id);
    assert!(
        listing.reactions.is_empty(),
        "reaction must not leak when message_id does not belong to session_id",
    );
}

/// Listing reactions under a session the caller cannot reach is a 404, not an
/// empty listing: the read is authorized at parent-session granularity, and an
/// unreachable session must stay indistinguishable from a missing one.
//
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn list_reactions_for_unreachable_session_is_not_found() {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let svc = make_service(
        StubSessionRepo::new(make_session(session_id, None)),
        StubMessageRepo::assistant(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
    );

    let err = svc
        .list_reactions(&make_ctx(), Uuid::new_v4(), message_id)
        .await
        .expect_err("an unreachable session must not list reactions");
    assert!(
        matches!(
            err,
            ChatEngineError::NotFound {
                resource: "session",
                ..
            }
        ),
        "expected a session NotFound, got: {err:?}",
    );
}

#[test]
fn ensure_feedback_capability_passes_when_present() {
    let now = OffsetDateTime::now_utc();
    let session = Session {
        session_id: Uuid::nil(),
        tenant_id: "t".to_string().into(),
        user_id: "u".to_string().into(),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: Some(serde_json::json!([
            { "name": "model", "value": "gpt-4" },
            { "name": "feedback", "value": true },
        ])),
        metadata: None,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: now,
        updated_at: now,
    };
    ensure_feedback_capability(&session).expect("passes");
}

#[test]
fn ensure_feedback_capability_rejects_when_array_missing() {
    let now = OffsetDateTime::now_utc();
    let session = Session {
        session_id: Uuid::nil(),
        tenant_id: "t".to_string().into(),
        user_id: "u".to_string().into(),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata: None,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: now,
        updated_at: now,
    };
    let err = ensure_feedback_capability(&session).unwrap_err();
    assert!(matches!(err, ChatEngineError::Conflict { .. }));
}

// --------------------------- Authz (PEP) tests ------------------------
//
// The reaction mutation path gates on the parent SESSION (UPDATE). All enforcer
// failure modes — an explicit PDP deny, an evaluation error, or a policy compile
// error — fail closed to `Forbidden` (HTTP 403) via `From<EnforcerError>`.
// The read path is trust-parent (unrestricted table, no PDP call).
// @cpt-cf-chat-engine-interface-pep

fn authz_fixture(enforcer: PolicyEnforcer) -> (ReactionService, Uuid, Uuid) {
    let session_id = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let session = make_session(
        session_id,
        Some(serde_json::json!([{ "name": "feedback", "value": true }])),
    );
    let svc = make_service_with_enforcer(
        StubSessionRepo::new(session),
        StubMessageRepo::assistant(session_id, message_id),
        Arc::new(StubReactionRepo::default()),
        enforcer,
    );
    (svc, session_id, message_id)
}

#[tokio::test]
async fn set_reaction_pdp_denied_returns_forbidden() {
    let (svc, session_id, message_id) = authz_fixture(test_support::enforcer_deny());
    let res = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::Like)
        .await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}

#[tokio::test]
async fn delete_reaction_pdp_denied_returns_forbidden() {
    // ReactionType::None routes through the delete branch — still gated on
    // the parent SESSION UPDATE, so a PDP deny fails closed to 403.
    let (svc, session_id, message_id) = authz_fixture(test_support::enforcer_deny());
    let res = svc
        .set_reaction(&make_ctx(), session_id, message_id, ReactionType::None)
        .await;
    assert!(matches!(res, Err(ChatEngineError::Forbidden { .. })));
}

// NOTE: `list_reactions` used to gate on REACTION (LIST) via the PDP. Reactions
// are now an unrestricted, trust-parent table (no owner columns): the read path
// no longer calls the enforcer — callers authorize the parent message in the
// same request (set_reaction authorizes before echoing; the batch reader is fed
// ids from an already-scoped message read). The former
// `list_reactions_{pdp_denied,evaluation_failed,compile_failed}_returns_forbidden`
// tests were removed with that gate; the mutation-path PDP tests above stay.

#[tokio::test]
async fn list_reactions_real_db_read_returns_empty() {
    // Exercise the real read path (`list_by_message` over in-memory SQLite):
    // a message with no reactions gets an empty, non-error listing — the
    // unrestricted query (post owner-column drop) executes end to end. The
    // parent session is owned by the caller, so the listing is authorized.
    let db = test_support::inmem_db().await;
    let svc = test_support::build_reaction_service(&db, test_support::enforcer_allow());
    let (tenant, user, session_id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    test_support::seed_session(&db, session_id, tenant, user).await;
    let ctx = test_support::ctx_for_subject(user, tenant);

    let message_id = Uuid::new_v4();
    let listing = svc
        .list_reactions(&ctx, session_id, message_id)
        .await
        .expect("empty scoped read succeeds");
    assert_eq!(listing.message_id, message_id);
    assert!(listing.reactions.is_empty());
}

// ===========================================================================
// Real-repo harness: set_reaction + list_for_messages over in-memory SQLite so
// the Sea-ORM reaction repo (upsert / get_by_pk / list) runs end-to-end.
// ===========================================================================

#[tokio::test]
async fn set_reaction_real_repo_upserts_then_lists() {
    use crate::domain::service::test_support::{
        build_reaction_service, build_session_service, ctx_for_subject, enforcer_allow, inmem_db,
        message_repo, seed_session,
    };

    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let ctx = ctx_for_subject(user, tenant);

    // Enable the feedback capability (persisted) so reactions are permitted.
    build_session_service(&db, enforcer_allow())
        .update_capabilities(
            &ctx,
            sid,
            vec![chat_engine_sdk::models::CapabilityValue {
                name: "feedback".into(),
                value: serde_json::json!(true),
            }],
        )
        .await
        .expect("enable feedback capability");

    // Seed a message to react to.
    let pair = message_repo(&db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id: sid,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(user.to_string()),
            parent_message_id: None,
            parts: vec![chat_engine_sdk::models::MessagePartInput {
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

    let rx = build_reaction_service(&db, enforcer_allow());
    let (resp, _mutation) = rx
        .set_reaction(&ctx, sid, pair.assistant_message_id, ReactionType::Like)
        .await
        .expect("set_reaction ok");
    assert_eq!(resp.reaction_type, ReactionType::Like);

    // Batch read back through the real repo.
    let by_msg = rx
        .list_for_messages(&ctx, &[pair.assistant_message_id])
        .await
        .expect("list_for_messages ok");
    assert!(by_msg.contains_key(&pair.assistant_message_id));

    // Point read of the same message's reactions.
    let listing = rx
        .list_reactions(&ctx, sid, pair.assistant_message_id)
        .await
        .expect("list_reactions ok");
    assert!(!listing.reactions.is_empty());

    // Clearing the reaction (None) exercises the repo delete path.
    let (cleared, _) = rx
        .set_reaction(&ctx, sid, pair.assistant_message_id, ReactionType::None)
        .await
        .expect("clear reaction ok");
    assert_eq!(cleared.reaction_type, ReactionType::None);
    let after = rx
        .list_reactions(&ctx, sid, pair.assistant_message_id)
        .await
        .expect("list after clear ok");
    assert!(after.reactions.is_empty(), "reaction removed after clear");
}

// ===========================================================================
// Ownership guard under the PDP the platform actually ships (tenant-only
// constraints). `message_reactions` is an unrestricted table, so the parent
// session is the entire authorization boundary for both writes and reads.
// @cpt-cf-chat-engine-nfr-authentication
// ===========================================================================

#[tokio::test]
async fn reactions_by_same_tenant_stranger_are_not_found_under_tenant_only_pdp() {
    use crate::domain::ports::NewUserMessage;
    use crate::domain::service::test_support::{
        build_reaction_service, build_session_service, ctx_for_subject, enforcer_allow,
        enforcer_allow_tenant_only, inmem_db, message_repo, seed_session,
    };

    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;
    let owner_ctx = ctx_for_subject(owner, tenant);

    build_session_service(&db, enforcer_allow())
        .update_capabilities(
            &owner_ctx,
            sid,
            vec![chat_engine_sdk::models::CapabilityValue {
                name: "feedback".into(),
                value: serde_json::json!(true),
            }],
        )
        .await
        .expect("enable feedback capability");

    let pair = message_repo(&db)
        .insert_user_and_assistant_stub(NewUserMessage {
            session_id: sid,
            tenant_id: Some(tenant.to_string()),
            user_id: Some(owner.to_string()),
            parent_message_id: None,
            parts: vec![chat_engine_sdk::models::MessagePartInput {
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

    let rx = build_reaction_service(&db, enforcer_allow_tenant_only());
    let stranger = ctx_for_subject(Uuid::new_v4(), tenant);

    let err = rx
        .set_reaction(
            &stranger,
            sid,
            pair.assistant_message_id,
            ReactionType::Like,
        )
        .await
        .expect_err("a same-tenant stranger must not react in another user's session");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    let err = rx
        .list_reactions(&stranger, sid, pair.assistant_message_id)
        .await
        .expect_err("a same-tenant stranger must not read another user's reactions");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    // Control: the owner's own write/read path is unaffected by the guard.
    rx.set_reaction(
        &owner_ctx,
        sid,
        pair.assistant_message_id,
        ReactionType::Like,
    )
    .await
    .expect("owner reacts in its own session");
    let listing = rx
        .list_reactions(&owner_ctx, sid, pair.assistant_message_id)
        .await
        .expect("owner lists its own reactions");
    assert!(!listing.reactions.is_empty());
}
