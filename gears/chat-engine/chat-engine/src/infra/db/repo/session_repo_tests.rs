use super::*;

use crate::domain::service::test_support::{inmem_db, session_repo};

fn new_session(tenant: Uuid, user: Uuid) -> NewSession {
    let now = OffsetDateTime::now_utc();
    NewSession {
        session_id: Uuid::new_v4(),
        tenant_id: tenant.to_string(),
        user_id: user.to_string(),
        client_id: None,
        session_type_id: None,
        metadata: None,
        created_at: now,
        updated_at: now,
    }
}

// insert_scoped must validate the row against the PDP-derived CREATE scope
// (scope_with_model), not skip validation (scope_unchecked). A constrained
// decision whose owner predicate the row does NOT satisfy is `Denied` →
// Forbidden, never a silent cross-scope write.
// @cpt-cf-chat-engine-interface-pep
#[tokio::test]
async fn insert_scoped_rejects_row_outside_constrained_scope() {
    let db = inmem_db().await;
    let repo = session_repo(&db);

    // PDP allows CREATE but constrains ownership to a DIFFERENT tenant than the
    // row being inserted.
    let scope = AccessScope::for_tenant(Uuid::new_v4());
    let err = repo
        .insert_scoped(&scope, new_session(Uuid::new_v4(), Uuid::new_v4()))
        .await
        .expect_err("insert must be denied when the row violates the scope");
    assert!(
        matches!(err, ChatEngineError::Forbidden { .. }),
        "Expected Forbidden for a row outside the constrained scope, got: {err:?}"
    );
}

// The validated path must not over-restrict: a row whose owner satisfies the
// scope's constraint is persisted normally (guards the Uuid-vs-Uuid predicate
// comparison the fix relies on).
// @cpt-cf-chat-engine-interface-pep
#[tokio::test]
async fn insert_scoped_persists_row_satisfying_constrained_scope() {
    let db = inmem_db().await;
    let repo = session_repo(&db);

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let inserted = repo
        .insert_scoped(&scope, new_session(tenant, Uuid::new_v4()))
        .await
        .expect("insert must succeed when the row satisfies the scope");

    let read_back = repo
        .find_by_id_scoped(&scope, inserted.session_id)
        .await
        .expect("scoped read must not error")
        .expect("the in-scope row must be readable back");
    assert_eq!(read_back.session_id, inserted.session_id);
}

// An unconstrained scope (PDP allowed with no row filter) leaves the insert
// unrestricted — the fix is a no-op on this path.
// @cpt-cf-chat-engine-interface-pep
#[tokio::test]
async fn insert_scoped_allows_any_row_under_unconstrained_scope() {
    let db = inmem_db().await;
    let repo = session_repo(&db);

    let scope = AccessScope::allow_all();
    repo.insert_scoped(&scope, new_session(Uuid::new_v4(), Uuid::new_v4()))
        .await
        .expect("unconstrained scope must not block the insert");
}

// A tenant-only scope — what the shipped policy plugins compile to, since
// neither emits an `owner_id` predicate — does NOT isolate users: the row of a
// same-tenant stranger is fully readable through it. This is why session
// ownership is enforced in the gear by `owner_guard::ensure_session_owner`
// rather than being left to the PDP-derived scope.
// @cpt-cf-chat-engine-nfr-authentication
#[tokio::test]
async fn tenant_only_scope_does_not_isolate_users() {
    let db = inmem_db().await;
    let repo = session_repo(&db);

    let tenant = Uuid::new_v4();
    let scope = AccessScope::for_tenant(tenant);
    let owner_row = repo
        .insert_scoped(&scope, new_session(tenant, Uuid::new_v4()))
        .await
        .expect("seed session owned by one user");

    // Same tenant, different subject: the scope is identical, so the read hits.
    let seen = repo
        .find_by_id_scoped(&scope, owner_row.session_id)
        .await
        .expect("read under tenant-only scope");
    assert!(
        seen.is_some(),
        "a tenant-only scope admits another user's row; ownership must be \
         enforced above the repo layer",
    );
}

// The legacy owner-filtered write path must MATCH the row it owns.
// `sessions.tenant_id` / `user_id` are UUID columns since
// `m20260417_000006_authz_owner_columns`, so a predicate built from the
// caller's id STRINGS matches nothing — Postgres rejects `uuid = text` and the
// SQLite driver stores the value as a BLOB — and the legitimate owner's write
// came back as `NotFound` (a 404 on `POST /sessions/{id}/switch-type`).
// @cpt-cf-chat-engine-principle-owner-denorm-invariant
#[tokio::test]
async fn legacy_owner_filtered_write_matches_the_owning_row() {
    use crate::domain::ports::NewSessionType;
    use crate::domain::service::test_support::{seed_session, session_type_repo, variant_repo};

    let db = inmem_db().await;
    let (tenant, user, sid) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    seed_session(&db, sid, tenant, user).await;

    let target_type = session_type_repo(&db)
        .insert(NewSessionType {
            session_type_id: Uuid::new_v4(),
            name: "target".to_owned(),
            plugin_instance_id: Some("gts.test.switch.v1~".to_owned()),
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        })
        .await
        .expect("seed target session type");

    let switched = variant_repo(&db)
        .update_session_type(
            &tenant.to_string(),
            &user.to_string(),
            sid,
            target_type.session_type_id,
            serde_json::json!([]),
        )
        .await
        .expect("the owner's switch-type must find its own session row");
    assert_eq!(switched.session_type_id, Some(target_type.session_type_id));

    // Control: a non-owner's ids must still match nothing.
    let err = variant_repo(&db)
        .update_session_type(
            &tenant.to_string(),
            &Uuid::new_v4().to_string(),
            sid,
            target_type.session_type_id,
            serde_json::json!([]),
        )
        .await
        .expect_err("a foreign user_id must not match the row");
    assert!(matches!(err, ChatEngineError::NotFound { .. }), "{err:?}");
}
