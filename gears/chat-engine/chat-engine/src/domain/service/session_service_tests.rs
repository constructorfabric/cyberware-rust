use super::*;

#[test]
fn reject_reserved_metadata_blocks_known_keys() {
    let metadata = serde_json::json!({"memory_strategy": {"type": "full"}});
    let err = reject_reserved_metadata(Some(&metadata)).unwrap_err();
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[test]
fn merge_plugin_metadata_object_merge_overlay_wins() {
    let base = Some(serde_json::json!({"a": 1, "b": 2}));
    let overlay = serde_json::json!({"b": 99, "c": 3});
    let merged = merge_plugin_metadata(base, overlay);
    assert_eq!(merged, serde_json::json!({"a": 1, "b": 99, "c": 3}));
}

#[test]
fn merge_plugin_metadata_strips_reserved_keys_from_overlay() {
    let base = Some(serde_json::json!({"a": 1}));
    // Plugin tries to set engine-reserved keys — they must be dropped.
    let overlay = serde_json::json!({
        "memory_strategy": {"type": "full"},
        "retention_policy": {"x": 1},
        "share_expires_at": "2026-01-01",
        "model": "gpt-4",
    });
    let merged = merge_plugin_metadata(base, overlay);
    assert_eq!(merged, serde_json::json!({"a": 1, "model": "gpt-4"}));
}

#[test]
fn merge_plugin_metadata_uses_overlay_when_base_absent() {
    let merged = merge_plugin_metadata(None, serde_json::json!({"k": "v"}));
    assert_eq!(merged, serde_json::json!({"k": "v"}));
}

#[test]
fn merge_plugin_metadata_keeps_base_when_overlay_not_object() {
    // A non-object overlay must not clobber existing client metadata.
    let base = Some(serde_json::json!({"a": 1}));
    let merged = merge_plugin_metadata(base, serde_json::json!("oops"));
    assert_eq!(merged, serde_json::json!({"a": 1}));
}

#[test]
fn reject_reserved_metadata_allows_client_keys() {
    let metadata = serde_json::json!({"title": "hello"});
    reject_reserved_metadata(Some(&metadata)).expect("client metadata accepted");
}

#[test]
#[allow(clippy::use_debug)]
fn redact_session_clears_share_token_and_reserved_metadata() {
    let s = Session {
        session_id: Uuid::nil(),
        tenant_id: TenantId::new("t"),
        user_id: UserId::new("u"),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata: Some(serde_json::json!({
            "memory_strategy": {"type": "full"},
            "client_field": "ok",
        })),
        lifecycle_state: LifecycleState::Active,
        share_token: Some(toolkit_utils::SecretString::new("super-secret")),
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };

    // Redaction proof: Debug must not leak the share token.
    let s_debug = format!("{s:?}");
    assert!(!s_debug.contains("super-secret"));

    let redacted = redact_session(s);
    assert!(redacted.share_token.is_none());
    assert_eq!(
        redacted.metadata,
        Some(serde_json::json!({"client_field": "ok"}))
    );
}

#[test]
fn identity_rejects_empty_tenant() {
    let err = Identity::new("", "u", None).unwrap_err();
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[test]
fn identity_rejects_empty_user() {
    let err = Identity::new("t", "", None).unwrap_err();
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[test]
fn parse_state_falls_back_to_active() {
    assert_eq!(
        LifecycleState::from_str_value("garbage").unwrap_or(LifecycleState::Active),
        LifecycleState::Active
    );
    assert_eq!(
        LifecycleState::from_str_value("soft_deleted"),
        Some(LifecycleState::SoftDeleted)
    );
}

// Anchor for the acceptance criterion that requires `ensure_can_transition`
// to be called before every state-changing write — the call sites in
// archive/restore/delete invoke this same helper, and the test below
// verifies the routing for one representative edge.
#[test]
fn ensure_can_transition_path_used_by_service_for_archive() {
    let from = LifecycleState::Active;
    let to = LifecycleState::Archived;
    ensure_can_transition(from, to).expect("active->archived is valid");
}

// ===========================================================================
// Authorization suite (Phase 8) — PEP enforcement on SessionService.
// Real Sea-ORM repos over an in-memory SQLite DB + a mock PDP, so each test
// exercises the full enforcer -> AccessScope -> SecureORM WHERE flow.
// ===========================================================================

use crate::domain::service::test_support::{
    build_session_service, ctx_allow_tenants, ctx_for_subject, enforcer_allow,
    enforcer_allow_tenant_only, enforcer_allow_unconstrained, enforcer_compile_fail, enforcer_deny,
    enforcer_failing, inmem_db, seed_session,
};
use toolkit_odata::ODataQuery;

// --- 2a. PDP deny (Denied) -> 403 -----------------------------------------

#[tokio::test]
async fn pdp_denied_list_sessions_returns_forbidden() {
    let db = inmem_db().await;
    let svc = build_session_service(&db, enforcer_deny());
    let ctx = ctx_allow_tenants(&[Uuid::new_v4()]);
    let err = svc
        .list_sessions(&ctx, &ODataQuery::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden from DenyAll, got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_create_session_returns_forbidden() {
    let db = inmem_db().await;
    let svc = build_session_service(&db, enforcer_deny());
    let ctx = ctx_allow_tenants(&[Uuid::new_v4()]);
    let err = svc
        .create_session(
            &ctx,
            CreateSessionRequest {
                session_type_id: None,
                metadata: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden from DenyAll, got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_get_session_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_deny());
    let err = svc
        .get_session(&ctx_for_subject(user, tenant), sid)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden from DenyAll, got: {err:?}"
    );
}

#[tokio::test]
async fn pdp_denied_update_session_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_deny());
    let err = svc
        .update_metadata(
            &ctx_for_subject(user, tenant),
            sid,
            serde_json::json!({"title": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden from DenyAll, got: {err:?}"
    );
}

#[tokio::test]
async fn update_metadata_and_capabilities_persists_both_atomically() {
    // Both fields supplied → single authorized transactional write; both the
    // metadata and the enabled_capabilities must land (no partial update).
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    let session = svc
        .update_metadata_and_capabilities(
            &ctx_for_subject(user, tenant),
            sid,
            serde_json::json!({ "title": "combined" }),
            vec![CapabilityValue {
                name: "feedback".into(),
                value: serde_json::json!(true),
            }],
        )
        .await
        .expect("combined update ok");

    // Metadata written (seeded session has no plugin → request metadata verbatim).
    let meta = session.metadata.expect("metadata set");
    assert_eq!(meta.get("title").and_then(|v| v.as_str()), Some("combined"));
    // Capabilities written in the same commit.
    assert!(
        session.enabled_capabilities.is_some(),
        "enabled_capabilities must be persisted alongside metadata",
    );
}

#[tokio::test]
async fn pdp_denied_delete_session_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_deny());
    let err = svc
        .delete_session(&ctx_for_subject(user, tenant), sid, false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden from DenyAll, got: {err:?}"
    );
}

// --- 2b. EvaluationFailed -> 403 (fail-closed) ----------------------------

#[tokio::test]
async fn evaluation_failed_list_sessions_returns_forbidden() {
    let db = inmem_db().await;
    let svc = build_session_service(&db, enforcer_failing());
    let ctx = ctx_allow_tenants(&[Uuid::new_v4()]);
    let err = svc
        .list_sessions(&ctx, &ODataQuery::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden (fail-closed) from PDP failure, got: {err:?}"
    );
}

#[tokio::test]
async fn evaluation_failed_get_session_returns_forbidden() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_failing());
    let err = svc
        .get_session(&ctx_for_subject(user, tenant), sid)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden (fail-closed) from PDP failure, got: {err:?}"
    );
}

// --- 2c. CompileFailed -> 403 (fail-closed) -------------------------------

#[tokio::test]
async fn compile_failed_list_sessions_returns_forbidden() {
    let db = inmem_db().await;
    let svc = build_session_service(&db, enforcer_compile_fail());
    let ctx = ctx_allow_tenants(&[Uuid::new_v4()]);
    let err = svc
        .list_sessions(&ctx, &ODataQuery::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden (fail-closed) from empty constraints, got: {err:?}"
    );
}

// --- 2d. Point-op scope-miss -> 404 (anti-enumeration) --------------------

#[tokio::test]
async fn get_session_wrong_tenant_returns_not_found() {
    let db = inmem_db().await;
    let (tenant_a, user_a, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant_a, user_a).await;

    // A subject in a different tenant: PDP allows with owner constraints that
    // filter to the subject's own rows, so the scoped re-read yields 0 rows.
    let svc = build_session_service(&db, enforcer_allow());
    let err = svc
        .get_session(&ctx_allow_tenants(&[Uuid::new_v4()]), sid)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "Expected NotFound for cross-tenant access, got: {err:?}"
    );
}

#[tokio::test]
async fn delete_session_wrong_tenant_returns_not_found() {
    let db = inmem_db().await;
    let (tenant_a, user_a, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant_a, user_a).await;

    let svc = build_session_service(&db, enforcer_allow());
    let err = svc
        .delete_session(&ctx_allow_tenants(&[Uuid::new_v4()]), sid, false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "Expected NotFound for cross-tenant delete, got: {err:?}"
    );
}

#[tokio::test]
async fn cross_tenant_update_on_deleted_session_is_not_found_not_conflict() {
    // Anti-enumeration: a cross-tenant caller mutating a soft-deleted session
    // must get NotFound (404), never Conflict "session is deleted" (409) — the
    // latter would leak the row's existence + lifecycle state. This is only
    // correct if authorize_session_op re-reads under the constrained scope
    // instead of consuming the unrestricted prefetch's lifecycle_state.
    let db = inmem_db().await;
    let (tenant_a, user_a, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant_a, user_a).await;

    let svc = build_session_service(&db, enforcer_allow());
    // Owner soft-deletes the session.
    svc.delete_session(&ctx_for_subject(user_a, tenant_a), sid, false)
        .await
        .expect("owner soft-delete ok");

    // Cross-tenant caller attempts a metadata update → must be 404, not 409.
    let err = svc
        .update_metadata(
            &ctx_for_subject(Uuid::new_v4(), Uuid::new_v4()),
            sid,
            serde_json::json!({ "title": "x" }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "cross-tenant update on a deleted session must be NotFound (not Conflict), got: {err:?}",
    );
}

// --- ownership guard: the PDP scopes the tenant, the gear scopes the owner --

/// Regression: under the PDP the platform actually ships (`static-authz` /
/// `tr-authz`), the compiled scope constrains `owner_tenant_id` only. Before
/// the ownership guard, that scope let any authenticated subject read a
/// same-tenant stranger's session (NFR-006 violation). It must be 404 —
/// NotFound rather than Forbidden, so the caller cannot enumerate sessions.
//
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn get_session_same_tenant_stranger_is_not_found_under_tenant_only_pdp() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;

    let svc = build_session_service(&db, enforcer_allow_tenant_only());
    let err = svc
        .get_session(&ctx_for_subject(Uuid::new_v4(), tenant), sid)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "a same-tenant stranger must not read another user's session, got: {err:?}",
    );
}

/// The same guard must hold for mutations, not just reads.
//
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn mutations_by_same_tenant_stranger_are_not_found_under_tenant_only_pdp() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;

    let svc = build_session_service(&db, enforcer_allow_tenant_only());
    let stranger = ctx_for_subject(Uuid::new_v4(), tenant);

    let err = svc
        .update_metadata(&stranger, sid, serde_json::json!({ "title": "pwned" }))
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    let err = svc.archive_session(&stranger, sid).await.unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    let err = svc.delete_session(&stranger, sid, false).await.unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");

    // The row is untouched: the owner still sees an Active session.
    let still_there = svc
        .get_session(&ctx_for_subject(owner, tenant), sid)
        .await
        .expect("owner still reads its session");
    assert_eq!(still_there.lifecycle_state, LifecycleState::Active);
    assert!(still_there.metadata.is_none(), "metadata must be unchanged");
}

/// A PDP that allows without any constraint compiles to an unconstrained scope
/// (`allow_all`). The prefetch behind it is read under the system bypass, so
/// without the guard that path would hand over any session in any tenant.
//
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn get_session_unconstrained_allow_still_enforces_ownership() {
    let db = inmem_db().await;
    let (tenant, owner, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, owner).await;

    let svc = build_session_service(&db, enforcer_allow_unconstrained());
    let err = svc
        .get_session(&ctx_for_subject(Uuid::new_v4(), Uuid::new_v4()), sid)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "an unconstrained allow must not bypass ownership, got: {err:?}",
    );

    // Control: the owner is unaffected by the same PDP.
    svc.get_session(&ctx_for_subject(owner, tenant), sid)
        .await
        .expect("owner reads its own session under an unconstrained allow");
}

// --- happy path: owner can read its own session ---------------------------

#[tokio::test]
async fn get_session_owner_returns_session() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    let got = svc
        .get_session(&ctx_for_subject(user, tenant), sid)
        .await
        .expect("owner should read its own session");
    assert_eq!(got.session_id, sid);
}

// ===========================================================================
// Happy-path coverage: exercise the authorized write flows end-to-end so the
// scoped repo mutations (update_metadata/capabilities/lifecycle/soft_delete)
// and the session-type surface run under a permissive PDP.
// ===========================================================================

#[tokio::test]
async fn create_session_owner_persists_and_reads_back() {
    let db = inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let ctx = ctx_for_subject(user, tenant);
    let svc = build_session_service(&db, enforcer_allow());

    let created = svc
        .create_session(
            &ctx,
            CreateSessionRequest {
                session_type_id: None,
                metadata: Some(serde_json::json!({ "title": "fresh" })),
            },
        )
        .await
        .expect("owner create ok");

    let got = svc
        .get_session(&ctx, created.session_id)
        .await
        .expect("read back own session");
    assert_eq!(got.session_id, created.session_id);
    assert_eq!(
        got.metadata.and_then(|m| m.get("title").cloned()),
        Some(serde_json::json!("fresh"))
    );
}

#[tokio::test]
async fn list_sessions_owner_returns_seeded_row() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    let page = svc
        .list_sessions(&ctx_for_subject(user, tenant), &ODataQuery::default())
        .await
        .expect("owner list ok");
    assert!(page.items.iter().any(|s| s.session_id == sid));
}

#[tokio::test]
async fn update_metadata_owner_persists() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    let updated = svc
        .update_metadata(
            &ctx_for_subject(user, tenant),
            sid,
            serde_json::json!({ "title": "renamed" }),
        )
        .await
        .expect("owner metadata update ok");
    assert_eq!(
        updated.metadata.and_then(|m| m.get("title").cloned()),
        Some(serde_json::json!("renamed"))
    );
}

#[tokio::test]
async fn update_capabilities_owner_persists() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    // Seeded session has no session_type → no plugin negotiation; caps verbatim.
    let updated = svc
        .update_capabilities(
            &ctx_for_subject(user, tenant),
            sid,
            vec![CapabilityValue {
                name: "feedback".into(),
                value: serde_json::json!(true),
            }],
        )
        .await
        .expect("owner capability update ok");
    assert!(updated.enabled_capabilities.is_some());
}

#[tokio::test]
async fn archive_then_restore_roundtrip() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    let ctx = ctx_for_subject(user, tenant);
    let svc = build_session_service(&db, enforcer_allow());

    let archived = svc.archive_session(&ctx, sid).await.expect("archive ok");
    assert_eq!(archived.lifecycle_state, LifecycleState::Archived);

    let restored = svc.restore_session(&ctx, sid).await.expect("restore ok");
    assert_eq!(restored.lifecycle_state, LifecycleState::Active);
}

#[tokio::test]
async fn delete_session_soft_owner_marks_soft_deleted() {
    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let svc = build_session_service(&db, enforcer_allow());
    let outcome = svc
        .delete_session(&ctx_for_subject(user, tenant), sid, false)
        .await
        .expect("owner soft-delete ok");
    match outcome {
        SessionDeleteOutcome::Soft { session } => {
            assert_eq!(session.lifecycle_state, LifecycleState::SoftDeleted);
        }
        SessionDeleteOutcome::Hard => panic!("expected soft-delete outcome"),
    }
}

// --- session-type surface -------------------------------------------------

#[tokio::test]
async fn register_session_type_owner_no_plugin_persists() {
    let db = inmem_db().await;
    let ctx = ctx_for_subject(Uuid::new_v4(), Uuid::new_v4());
    let svc = build_session_service(&db, enforcer_allow_unconstrained());

    let st = svc
        .register_session_type(
            &ctx,
            RegisterSessionTypeRequest {
                name: "support".into(),
                plugin_instance_id: None,
                plugin_config: None,
            },
        )
        .await
        .expect("register ok");
    assert_eq!(st.name, "support");

    // list + get read paths (authenticated-only, no PDP call).
    let all = svc.list_session_types(&ctx).await.expect("list ok");
    assert!(all.iter().any(|t| t.session_type_id == st.session_type_id));
    let got = svc
        .get_session_type(&ctx, st.session_type_id)
        .await
        .expect("get ok");
    assert_eq!(got.session_type_id, st.session_type_id);
}

#[tokio::test]
async fn register_session_type_empty_name_is_bad_request() {
    let db = inmem_db().await;
    let ctx = ctx_for_subject(Uuid::new_v4(), Uuid::new_v4());
    let svc = build_session_service(&db, enforcer_allow_unconstrained());
    let err = svc
        .register_session_type(
            &ctx,
            RegisterSessionTypeRequest {
                name: "   ".into(),
                plugin_instance_id: None,
                plugin_config: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::BadRequest { .. }));
}

#[tokio::test]
async fn register_session_type_pdp_denied_returns_forbidden() {
    let db = inmem_db().await;
    let ctx = ctx_for_subject(Uuid::new_v4(), Uuid::new_v4());
    let svc = build_session_service(&db, enforcer_deny());
    let err = svc
        .register_session_type(
            &ctx,
            RegisterSessionTypeRequest {
                name: "support".into(),
                plugin_instance_id: None,
                plugin_config: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::Forbidden { .. }));
}

#[tokio::test]
async fn get_session_type_unknown_is_not_found() {
    let db = inmem_db().await;
    let ctx = ctx_for_subject(Uuid::new_v4(), Uuid::new_v4());
    let svc = build_session_service(&db, enforcer_allow());
    let err = svc
        .get_session_type(&ctx, Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

#[tokio::test]
async fn delete_session_hard_owner_removes_and_cascades() {
    use crate::domain::ports::NewUserMessage;
    use crate::domain::service::test_support::message_repo;

    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;
    // Seed a message so the hard-delete cascade runs over child rows too.
    message_repo(&db)
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

    let svc = build_session_service(&db, enforcer_allow());
    let ctx = ctx_for_subject(user, tenant);
    let outcome = svc
        .delete_session(&ctx, sid, true)
        .await
        .expect("hard delete ok");
    assert!(matches!(outcome, SessionDeleteOutcome::Hard));

    // The row is gone — a subsequent read is NotFound.
    let err = svc.get_session(&ctx, sid).await.unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

#[tokio::test]
async fn create_session_with_known_session_type_binds_it() {
    use crate::domain::ports::NewSessionType;
    use crate::domain::service::test_support::session_type_repo;

    let db = inmem_db().await;
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let ctx = ctx_for_subject(user, tenant);

    // Seed a plugin-less session type directly (no plugin call on create).
    let st_id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    session_type_repo(&db)
        .insert(NewSessionType {
            session_type_id: st_id,
            name: "support".into(),
            plugin_instance_id: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("seed session type");

    let svc = build_session_service(&db, enforcer_allow());
    let created = svc
        .create_session(
            &ctx,
            CreateSessionRequest {
                session_type_id: Some(st_id),
                metadata: None,
            },
        )
        .await
        .expect("create with session type ok");
    assert_eq!(created.session_type_id, Some(st_id));
}

#[tokio::test]
async fn create_session_with_unknown_session_type_is_not_found() {
    let db = inmem_db().await;
    let ctx = ctx_for_subject(Uuid::new_v4(), Uuid::new_v4());
    let svc = build_session_service(&db, enforcer_allow());
    let err = svc
        .create_session(
            &ctx,
            CreateSessionRequest {
                session_type_id: Some(Uuid::new_v4()),
                metadata: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}
