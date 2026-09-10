//! Cross-user isolation over the real `SQLite` stack.
//!
//! Every other message test drives the services as their owner. This one asks
//! the opposite question: with a full conversation persisted by user A —
//! including the assistant row written by the streaming pipeline under an
//! internal-write bypass — what can user B reach?
//!
//! It runs against `enforcer_allow_tenant_only`, the PDP shape the platform
//! actually ships: `static-authz` and `tr-authz` constrain `owner_tenant_id`
//! and never `owner_id`, so a same-tenant stranger's scope admits every row in
//! the tenant and only the gear's own ownership enforcement stands between
//! them and someone else's conversation.
//!
//! The owner-side assertions matter just as much: the caller clamp rides into
//! the SQL `WHERE`, so a pipeline-written row whose owner columns were not
//! stamped would silently vanish from its own owner's history.
//
// @cpt-cf-chat-engine-nfr-authentication
// @cpt-cf-chat-engine-design-auth-model

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

mod common;

use std::sync::Arc;

use chat_engine::domain::error::ChatEngineError;
use chat_engine::domain::export::{ExportFormat, ExportStorage, StubExportStorage};
use chat_engine::domain::service::export_service::ExportService;
use chat_engine::domain::service::intelligence_service::IntelligenceService;
use chat_engine::domain::service::message_service::{MessageService, SendMessageRequest};
use chat_engine::domain::service::plugin_service::PluginService;
use chat_engine::domain::service::variant_service::VariantService;
use chat_engine::infra::db::entity::message::MessageRole;
use chat_engine::infra::db::repo::variant_repo::SeaVariantRepo;
use chat_engine_sdk::models::{MessagePartInput, MessagePartType};
use chat_engine_sdk::{
    ChatEngineBackendPlugin, StreamingChunkEvent, StreamingCompleteEvent, StreamingEvent,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use toolkit::ClientHub;
use toolkit::client_hub::ClientScope;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use common::db::{self, DbHarness};
use common::{FakePlugin, FakePluginScript};

const PLUGIN_ID: &str = "gts.test.isolation.v1~";

fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

fn service(harness: &DbHarness) -> MessageService {
    let hub = Arc::new(ClientHub::new());
    let plugin: Arc<dyn ChatEngineBackendPlugin> = FakePlugin::new(
        PLUGIN_ID,
        FakePluginScript::Events(vec![
            StreamingEvent::Chunk(StreamingChunkEvent {
                message_id: Uuid::nil(),
                chunk: "the secret answer".into(),
            }),
            StreamingEvent::Complete(StreamingCompleteEvent {
                message_id: Uuid::nil(),
                metadata: None,
                file_citations: vec![],
                link_citations: vec![],
                references: vec![],
            }),
        ]),
    );
    hub.register_scoped::<dyn ChatEngineBackendPlugin>(ClientScope::gts_id(PLUGIN_ID), plugin);

    MessageService::new(
        Arc::clone(&harness.sessions),
        Arc::clone(&harness.session_types),
        Arc::clone(&harness.messages),
        PluginService::new(hub, Arc::clone(&harness.plugin_configs)),
        common::authz::enforcer_allow_tenant_only(),
    )
}

/// The session-anchored services: every one of them takes a `session_id`
/// straight off the request path, so the ownership guard inside their
/// `authorize_session_op` is the only thing standing between a stranger and
/// the conversation's contents.
struct SessionScopedServices {
    export: ExportService,
    intelligence: IntelligenceService,
    variants: VariantService,
}

fn session_scoped_services(harness: &DbHarness) -> SessionScopedServices {
    let hub = Arc::new(ClientHub::new());
    let plugins = PluginService::new(hub, Arc::clone(&harness.plugin_configs));
    let storage: Arc<dyn ExportStorage> = Arc::new(StubExportStorage);

    SessionScopedServices {
        export: ExportService::new(
            Arc::clone(&harness.sessions),
            Arc::clone(&harness.messages),
            storage,
            common::authz::enforcer_allow_tenant_only(),
        ),
        intelligence: IntelligenceService::new(
            Arc::clone(&harness.sessions),
            Arc::clone(&harness.session_types),
            Arc::clone(&harness.messages),
            plugins.clone(),
            common::authz::enforcer_allow_tenant_only(),
        ),
        variants: VariantService::new(
            Arc::clone(&harness.sessions),
            Arc::clone(&harness.session_types),
            Arc::clone(&harness.messages),
            Arc::new(SeaVariantRepo::new(Arc::clone(&harness.db))),
            plugins,
            Arc::new(service(harness)),
            common::authz::enforcer_allow_tenant_only(),
        ),
    }
}

/// Persist a complete user + assistant exchange through the real streaming
/// pipeline, so the assistant row is written the way production writes it.
async fn seed_conversation(
    harness: &DbHarness,
    svc: &MessageService,
    owner: &SecurityContext,
    tenant: Uuid,
    user: Uuid,
) -> (Uuid, Uuid, Uuid) {
    let session_type_id = db::seed_session_type(harness, PLUGIN_ID).await;
    let session_id = db::seed_active_session(
        harness,
        &tenant.to_string(),
        &user.to_string(),
        session_type_id,
    )
    .await;

    let mut stream = svc
        .send_message(
            SendMessageRequest {
                session_id,
                parts: vec![MessagePartInput {
                    part_type: MessagePartType::Text,
                    content: serde_json::json!({"text": "what is the secret?"}),
                    file_citations: vec![],
                    link_citations: vec![],
                    references: vec![],
                }],
                file_ids: vec![],
                parent_message_id: None,
                capabilities: None,
                metadata: None,
            },
            owner,
            CancellationToken::new(),
        )
        .await
        .expect("owner sends a message");
    while stream.next().await.is_some() {}

    let rows = db::list_messages(&harness.db, session_id).await;
    let user_message_id = rows
        .iter()
        .find(|m| matches!(m.role, MessageRole::User))
        .expect("user row persisted")
        .message_id;
    let assistant_message_id =
        db::wait_for_finalize(&harness.db, session_id, std::time::Duration::from_secs(2))
            .await
            .message_id;

    (session_id, user_message_id, assistant_message_id)
}

/// The owner must see the whole exchange — including the pipeline-written
/// assistant row, which is inserted under an internal-write bypass and must
/// still carry the session's owner pair to survive the caller clamp.
#[tokio::test]
async fn owner_reads_its_own_conversation_including_the_assistant_row() {
    let harness = db::setup_sqlite().await;
    let svc = service(&harness);
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let owner = ctx(user, tenant);
    let (session_id, _user_msg, assistant_id) =
        seed_conversation(&harness, &svc, &owner, tenant, user).await;

    let history = svc
        .list_active_messages(&owner, session_id, None)
        .await
        .expect("owner lists its own history");
    assert!(
        history.iter().any(|m| m.message_id == assistant_id),
        "the assistant row must not be scoped away from its own owner",
    );

    let msg = svc
        .resolve_owned_message(&owner, assistant_id)
        .await
        .expect("owner reads the assistant message");
    assert_eq!(msg.message_id, assistant_id);
}

/// A different user in the SAME tenant — the case the shipped PDP cannot
/// distinguish — must reach nothing: no history, no point read, no delete.
#[tokio::test]
async fn same_tenant_stranger_reaches_no_message_surface() {
    let harness = db::setup_sqlite().await;
    let svc = service(&harness);
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let owner = ctx(user, tenant);
    let (session_id, user_msg, assistant_id) =
        seed_conversation(&harness, &svc, &owner, tenant, user).await;

    let stranger = ctx(Uuid::new_v4(), tenant);

    let history = svc
        .list_active_messages(&stranger, session_id, None)
        .await
        .expect("the list decision is allowed; the rows are what must be scoped away");
    assert!(
        history.is_empty(),
        "a same-tenant stranger listed {} message(s) from another user's session",
        history.len(),
    );

    for (label, id) in [("user", user_msg), ("assistant", assistant_id)] {
        let err = svc
            .resolve_owned_message(&stranger, id)
            .await
            .expect_err("point read of a foreign message must fail");
        assert!(
            matches!(err, ChatEngineError::NotFound { .. }),
            "{label} message point read must be 404, got: {err:?}",
        );

        let err = svc
            .resolve_owned_message_for_delete(&stranger, id)
            .await
            .expect_err("delete-authorized read of a foreign message must fail");
        assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");
    }

    let err = svc
        .delete_message_cascade(&stranger, session_id, assistant_id)
        .await
        .expect_err("cascade delete in a foreign session must fail");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    let posted = svc
        .send_message(
            SendMessageRequest {
                session_id,
                parts: vec![MessagePartInput {
                    part_type: MessagePartType::Text,
                    content: serde_json::json!({"text": "injected"}),
                    file_citations: vec![],
                    link_citations: vec![],
                    references: vec![],
                }],
                file_ids: vec![],
                parent_message_id: None,
                capabilities: None,
                metadata: None,
            },
            &stranger,
            CancellationToken::new(),
        )
        .await;
    match posted {
        Ok(_) => panic!("posting into a foreign session must fail"),
        Err(err) => assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}"),
    }

    // Nothing the stranger attempted may have mutated the conversation.
    let rows = db::list_messages(&harness.db, session_id).await;
    assert_eq!(rows.len(), 2, "the owner's exchange must be intact");
    let owner_history = svc
        .list_active_messages(&owner, session_id, None)
        .await
        .expect("owner still reads its history");
    assert!(owner_history.iter().any(|m| m.message_id == assistant_id));
}

/// A caller from another tenant must be equally blind, and must not be able to
/// tell a foreign session from a missing one.
#[tokio::test]
async fn cross_tenant_stranger_reaches_no_message_surface() {
    let harness = db::setup_sqlite().await;
    let svc = service(&harness);
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let owner = ctx(user, tenant);
    let (session_id, _user_msg, assistant_id) =
        seed_conversation(&harness, &svc, &owner, tenant, user).await;

    let outsider = ctx(Uuid::new_v4(), Uuid::new_v4());

    let history = svc
        .list_active_messages(&outsider, session_id, None)
        .await
        .expect("list is decided by the PDP, rows by the scope");
    assert!(history.is_empty(), "cross-tenant history must be empty");

    let err = svc
        .resolve_owned_message(&outsider, assistant_id)
        .await
        .expect_err("cross-tenant point read must fail");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");
}

/// The session-anchored surfaces take `session_id` from the request path, so
/// each one is probed directly rather than through a message-id lookup that
/// would already have failed.
#[tokio::test]
async fn same_tenant_stranger_reaches_no_session_scoped_message_surface() {
    let harness = db::setup_sqlite().await;
    let svc = service(&harness);
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let owner = ctx(user, tenant);
    let (session_id, _user_msg, assistant_id) =
        seed_conversation(&harness, &svc, &owner, tenant, user).await;

    let services = session_scoped_services(&harness);
    let stranger = ctx(Uuid::new_v4(), tenant);

    let err = services
        .export
        .export(&stranger, session_id, ExportFormat::Json, false)
        .await
        .expect_err("exporting a foreign session must fail");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    let err = services
        .export
        .create_share(&stranger, session_id, None)
        .await
        .expect_err("sharing a foreign session must fail");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    match services
        .intelligence
        .summarize_session(&stranger, session_id, CancellationToken::new())
        .await
    {
        Ok(_) => panic!("summarizing a foreign session must fail"),
        Err(err) => assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}"),
    }

    let err = services
        .variants
        .list_variants(&stranger, session_id, assistant_id)
        .await
        .expect_err("listing variants in a foreign session must fail");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    // Control: the owner's export still carries the conversation text, so the
    // guard is rejecting the caller rather than emptying the content.
    let exported = services
        .export
        .export(&owner, session_id, ExportFormat::Json, false)
        .await
        .expect("owner exports its own session");
    assert!(
        exported.message_count > 0,
        "the owner's export must still contain the conversation",
    );
}
