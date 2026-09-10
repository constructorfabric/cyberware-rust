//! Unit tests for the session-ownership guard.
//
// @cpt-cf-chat-engine-nfr-authentication

use super::*;

use chat_engine_sdk::models::{LifecycleState, TenantId, UserId};
use time::OffsetDateTime;
use toolkit_security::ScopeConstraint;

fn session(tenant_id: Uuid, user_id: Uuid) -> Session {
    let now = OffsetDateTime::now_utc();
    Session {
        session_id: Uuid::new_v4(),
        tenant_id: TenantId::new(tenant_id.to_string()),
        user_id: UserId::new(user_id.to_string()),
        client_id: None,
        session_type_id: None,
        enabled_capabilities: None,
        metadata: None,
        lifecycle_state: LifecycleState::Active,
        share_token: None,
        created_at: now,
        updated_at: now,
    }
}

fn ctx(subject_id: Uuid, tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

#[test]
fn owner_passes() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    ensure_session_owner(&ctx(user, tenant), &session(tenant, user))
        .expect("the owning subject must pass the guard");
}

/// The regression this guard exists for: a same-tenant stranger. The PDP's
/// tenant-only constraint admits this caller, so the gear must reject it.
#[test]
fn same_tenant_stranger_is_not_found() {
    let tenant = Uuid::new_v4();
    let err = ensure_session_owner(
        &ctx(Uuid::new_v4(), tenant),
        &session(tenant, Uuid::new_v4()),
    )
    .expect_err("a foreign user_id must not reach the session");
    assert!(
        matches!(err, ChatEngineError::NotFound { .. }),
        "ownership mismatch must be 404, not 403 (anti-enumeration): {err:?}"
    );
}

/// Same subject id in a different tenant — covers the unconstrained-allow case
/// where the PDP returns no tenant clamp at all.
#[test]
fn cross_tenant_same_subject_is_not_found() {
    let user = Uuid::new_v4();
    let err = ensure_session_owner(&ctx(user, Uuid::new_v4()), &session(Uuid::new_v4(), user))
        .expect_err("a foreign tenant must not reach the session");
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

/// A corrupt (non-UUID) owner value must fail closed rather than fall back to
/// a string comparison.
#[test]
fn non_uuid_owner_fails_closed() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let mut row = session(tenant, user);
    row.user_id = UserId::new("not-a-uuid");
    let err = ensure_session_owner(&ctx(user, tenant), &row)
        .expect_err("an unparseable owner id must fail closed");
    assert!(matches!(err, ChatEngineError::NotFound { .. }));
}

/// A denial is deliberately indistinguishable from "no such session" on the
/// wire (404, ADR-0021), so the WARN is the only signal an operator has that a
/// caller was refused someone else's session. Pin that it carries the session
/// and the rejected subject.
#[test]
#[tracing_test::traced_test]
fn ownership_denial_is_logged_for_operators() {
    let tenant = Uuid::new_v4();
    let row = session(tenant, Uuid::new_v4());
    let caller = ctx(Uuid::new_v4(), tenant);

    ensure_session_owner(&caller, &row).expect_err("a stranger must be refused");

    assert!(
        logs_contain("session ownership check failed"),
        "the denial must be logged",
    );
    assert!(
        logs_contain(&format!("session_id={}", row.session_id)),
        "the log must name the session that was refused",
    );
    assert!(
        logs_contain(&format!("subject_id={}", caller.subject_id())),
        "the log must name the rejected subject",
    );
    assert!(
        logs_contain(&format!("subject_tenant_id={tenant}")),
        "the log must name the rejected subject's tenant",
    );
}

/// The owner's path must stay silent — a WARN per successful read would be
/// noise, and would bury the real denials.
#[test]
#[tracing_test::traced_test]
fn an_authorized_read_logs_nothing() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    ensure_session_owner(&ctx(user, tenant), &session(tenant, user)).expect("owner passes");
    assert!(
        !logs_contain("session ownership check failed"),
        "an authorized read must not log a denial",
    );
}

// --------------------------------------------------------------------------
// caller_scope
// --------------------------------------------------------------------------

fn owner_values(scope: &AccessScope) -> Vec<Uuid> {
    scope.all_uuid_values_for(pep_properties::OWNER_ID)
}

/// An unconstrained allow must collapse to the caller's own owner pair rather
/// than to every row in the table.
#[test]
fn caller_scope_pins_both_halves_of_an_unconstrained_scope() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let scope = caller_scope(&ctx(user, tenant), &AccessScope::allow_all());

    assert!(!scope.is_unconstrained() && !scope.is_deny_all());
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, user));
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant));
}

/// The shipped-PDP shape: a tenant clamp and nothing else. The tenant filter
/// is left as the PDP wrote it and the owner clamp is added.
#[test]
fn caller_scope_adds_owner_to_a_tenant_only_scope() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let scope = caller_scope(&ctx(user, tenant), &AccessScope::for_tenant(tenant));

    assert_eq!(owner_values(&scope), vec![user]);
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant));
}

/// A PDP tenant predicate must not be replaced by the caller's own tenant: a
/// policy may legitimately scope to a subtree or a set of tenants.
#[test]
fn caller_scope_keeps_an_existing_tenant_predicate() {
    let caller_tenant = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let pdp = AccessScope::for_tenants(vec![caller_tenant, other_tenant]);

    let scope = caller_scope(&ctx(user, caller_tenant), &pdp);
    let tenants = scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID);
    assert!(tenants.contains(&caller_tenant) && tenants.contains(&other_tenant));
}

/// Intersection, not replacement: a policy granting someone else's rows must
/// not be widened back to the caller — the constraint is dropped, and with no
/// constraint left the scope is deny-all.
#[test]
fn caller_scope_drops_a_constraint_scoped_to_another_owner() {
    let tenant = Uuid::new_v4();
    let pdp = AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, Uuid::new_v4()),
    ]));

    let scope = caller_scope(&ctx(Uuid::new_v4(), tenant), &pdp);
    assert!(
        scope.is_deny_all(),
        "a scope granting another owner's rows must not survive the clamp",
    );
}

/// A policy that already grants the caller (among others) is narrowed to the
/// caller alone.
#[test]
fn caller_scope_narrows_a_multi_owner_grant() {
    let tenant = Uuid::new_v4();
    let user = Uuid::new_v4();
    let pdp = AccessScope::single(ScopeConstraint::new(vec![ScopeFilter::in_uuids(
        pep_properties::OWNER_ID,
        vec![user, Uuid::new_v4()],
    )]));

    let scope = caller_scope(&ctx(user, tenant), &pdp);
    assert_eq!(owner_values(&scope), vec![user]);
}

#[test]
fn caller_scope_keeps_deny_all_denied() {
    let scope = caller_scope(
        &ctx(Uuid::new_v4(), Uuid::new_v4()),
        &AccessScope::deny_all(),
    );
    assert!(scope.is_deny_all());
}
