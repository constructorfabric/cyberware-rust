#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
use super::*;

use authz_resolver_sdk::models::{BarrierMode, TenantContext, TenantMode};
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{
    DenyReason as RbacDenyReason, EffectivePermission, PermissionRule, PermissionScopeType,
    Scope as RbacScope,
};
use uuid::Uuid;

use crate::domain::deny::error_codes::INSUFFICIENT_PERMISSIONS_V1;
use crate::test_support::trusted_actors::{
    AM_SYSTEM_ACTOR_UUID as TEST_AM_SYSTEM_ACTOR_UUID,
    RMS_SYSTEM_ACTOR_UUID as TEST_RMS_SYSTEM_ACTOR_UUID,
    RMS_SYSTEM_SUBJECT_TYPE as TEST_RMS_SYSTEM_SUBJECT_TYPE, trusted_actors as test_trusted_actors,
};
use crate::test_support::{EvaluationRequestBuilder, InMemoryRbacServiceClient};

/// Canonical GTS subject-type identifiers — test inputs exercising the GTS-tag
/// path through `map_subject_type` (raw-claim path is covered separately).
const SUBJECT_TYPE_USER: &str = "gts.cf.core.security.subject_user.v1~";
const SUBJECT_TYPE_SERVICE_PRINCIPAL: &str = "gts.cf.core.security.subject_service_principal.v1~";

fn evaluator(rbac: Arc<InMemoryRbacServiceClient>) -> PolicyEvaluator {
    PolicyEvaluator::new(rbac as Arc<dyn RbacServiceClientV1>, test_trusted_actors())
}

// ------------ Subject-type mapping ------------

#[test]
fn u_14_user_subject_maps_to_principal_user() {
    let result = PolicyEvaluator::map_subject_type(Some(SUBJECT_TYPE_USER));
    match result {
        Ok(PrincipalType::User) => {}
        other => panic!("expected Ok(User), got {other:?}"),
    }
}

#[test]
fn u_15_service_principal_maps_to_principal_service_principal() {
    let result = PolicyEvaluator::map_subject_type(Some(SUBJECT_TYPE_SERVICE_PRINCIPAL));
    match result {
        Ok(PrincipalType::ServicePrincipal) => {}
        other => panic!("expected Ok(ServicePrincipal), got {other:?}"),
    }
}

#[test]
fn raw_idp_user_claim_maps_to_principal_user() {
    // The real-world case: Keycloak emits `user_type=user`, so the plugin sees
    // the bare `"user"` — it must map to User, not reject as unknown.
    match PolicyEvaluator::map_subject_type(Some("user")) {
        Ok(PrincipalType::User) => {}
        other => panic!("expected Ok(User) for raw 'user', got {other:?}"),
    }
    match PolicyEvaluator::map_subject_type(Some("service")) {
        Ok(PrincipalType::ServicePrincipal) => {}
        other => panic!("expected Ok(ServicePrincipal) for raw 'service', got {other:?}"),
    }
}

#[test]
fn absent_subject_type_defaults_to_user() {
    // Mirrors RBAC: an absent subject_type defaults to User rather than
    // failing closed (DESIGN §3.5 deviation, documented on map_subject_type).
    match PolicyEvaluator::map_subject_type(None) {
        Ok(PrincipalType::User) => {}
        other => panic!("expected Ok(User) for absent subject_type, got {other:?}"),
    }
}

#[test]
fn u_09_group_subject_type_rejected_as_unknown() {
    // Use a `.contains()` split rather than exact equality: the test's claim
    // is "this realistic-looking id is rejected as unknown", which stays
    // accurate even if the message format changes; if `subject_group` ever
    // becomes a supported subject type, this test breaks loudly (the error
    // path won't fire at all), not subtly via a string-mismatch.
    let id = "gts.cf.core.security.subject_group.v1~";
    let result = PolicyEvaluator::map_subject_type(Some(id));
    match result {
        Err(err @ PluginError::UnknownSubjectType { .. }) => {
            let msg = err.to_string();
            assert!(
                msg.contains("unknown subject type") && msg.contains(id),
                "expected message to flag the id as unknown, got {msg:?}"
            );
        }
        other => panic!("expected unknown-subject-type error, got {other:?}"),
    }
}

#[test]
fn arbitrary_unknown_value_rejected_with_value_in_message() {
    let result = PolicyEvaluator::map_subject_type(Some("definitely.not.a.real.type"));
    match result {
        Err(err @ PluginError::UnknownSubjectType { .. }) => {
            assert_eq!(
                err.to_string(),
                "unknown subject type: definitely.not.a.real.type"
            );
        }
        other => panic!("expected unknown-subject-type error, got {other:?}"),
    }
}

// ------------ Scope translation ------------

fn context_with_tenant(tenant_context: Option<TenantContext>) -> EvaluationRequestContext {
    EvaluationRequestContext {
        tenant_context,
        token_scopes: Vec::new(),
        require_constraints: false,
        capabilities: Vec::new(),
        supported_properties: Vec::new(),
        bearer_token: None,
    }
}

#[test]
fn scope_translation_no_tenant_context_maps_to_root() {
    let ctx = context_with_tenant(None);
    assert_eq!(evaluation_context_to_scope(&ctx), RbacScope::root());
}

#[test]
fn scope_translation_tenant_context_no_root_id_maps_to_root() {
    let ctx = context_with_tenant(Some(TenantContext {
        mode: TenantMode::Subtree,
        root_id: None,
        barrier_mode: BarrierMode::Respect,
        tenant_status: None,
    }));
    assert_eq!(evaluation_context_to_scope(&ctx), RbacScope::root());
}

#[test]
fn scope_translation_tenant_context_with_root_id_maps_to_tenant() {
    let tenant_id = Uuid::new_v4();
    let ctx = context_with_tenant(Some(TenantContext {
        mode: TenantMode::Subtree,
        root_id: Some(tenant_id),
        barrier_mode: BarrierMode::Respect,
        tenant_status: None,
    }));
    assert_eq!(
        evaluation_context_to_scope(&ctx),
        RbacScope::tenant(tenant_id)
    );
}

// ------------ evaluate_permissions ------------

fn allowed_grant() -> EffectivePermission {
    EffectivePermission::new(
        PermissionRule::new("read", "gts.cf.core.resources.test.v1~"),
        Uuid::new_v4(), // role_definition_id
        Uuid::new_v4(), // role_assignment_id
        "viewer",
        RbacScope::root(),
        false,
    )
}

#[tokio::test]
async fn t_09_rbac_denied_returns_insufficient_permissions() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_denied(
        RbacDenyReason::NoMatchingPermission,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));

    let request = EvaluationRequestBuilder::default().build();
    let outcome = evaluator
        .evaluate_permissions(&request)
        .await
        .expect("Denied is Ok(PolicyOutcome::Denied), not Err");

    match outcome {
        PolicyOutcome::Denied(response) => {
            assert!(!response.decision);
            let reason = response.context.deny_reason.expect("populated");
            assert_eq!(reason.error_code, INSUFFICIENT_PERMISSIONS_V1);
            let details = reason.details.expect("details populated");
            assert!(
                details.contains("read") && details.contains("gts.cf.core.resources.test.v1~"),
                "details should name op + resource_type: {details}"
            );
        }
        PolicyOutcome::Allowed(_) => panic!("expected Denied"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn u_06_rbac_error_returns_service_unavailable() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("simulated"),
    ));
    let evaluator = evaluator(Arc::clone(&rbac));

    let request = EvaluationRequestBuilder::default().build();
    let result = evaluator.evaluate_permissions(&request).await;
    match result {
        Err(err @ PluginError::RbacUnavailable) => {
            assert_eq!(err.to_string(), "rbac service unavailable");
        }
        other => panic!("expected RbacUnavailable, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

#[tokio::test]
async fn rbac_dependency_unavailable_also_maps_to_service_unavailable() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::dependency_unavailable("tenant-resolver"),
    ));
    let evaluator = evaluator(rbac);

    let request = EvaluationRequestBuilder::default().build();
    match evaluator.evaluate_permissions(&request).await {
        Err(err @ PluginError::RbacUnavailable) => {
            assert_eq!(err.to_string(), "rbac service unavailable");
        }
        other => panic!("expected RbacUnavailable, got {other:?}"),
    }
}

#[tokio::test]
async fn u_16_additive_union_pass_through() {
    // Two grants representing what RBAC produced after additive union of
    // two roles. The plugin must pass them through verbatim — it does
    // NOT reinterpret the aggregation rules.
    let grants = vec![allowed_grant(), allowed_grant()];
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        grants.clone(),
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(rbac);

    let request = EvaluationRequestBuilder::default().build();
    match evaluator.evaluate_permissions(&request).await.unwrap() {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 2);
            assert_eq!(granted.scope_type, PermissionScopeType::Global);
        }
        PolicyOutcome::Denied(_) => panic!("expected Allowed"),
    }
}

#[tokio::test]
async fn u_17_intra_role_not_permissions_pass_through() {
    // RBAC has already applied intra-role not_permissions; the plugin
    // receives the effective grant set and returns it verbatim.
    let grants = vec![allowed_grant()];
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        grants,
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(rbac);

    let request = EvaluationRequestBuilder::default().build();
    match evaluator.evaluate_permissions(&request).await.unwrap() {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 1);
        }
        PolicyOutcome::Denied(_) => panic!("expected Allowed"),
    }
}

#[tokio::test]
async fn u_18_cross_role_isolation_pass_through() {
    // Two grants — one for write from Role-A, one for read from a
    // separate role. The plugin returns both regardless of whether some
    // other role has not_permissions on the operation.
    let grants = vec![allowed_grant(), allowed_grant()];
    let scope_type = PermissionScopeType::TenantSubtree {
        root_tenant_id: Uuid::new_v4(),
    };
    // This unit test pins raw policy-adapter pass-through rather than scope
    // provenance, so opt into an exact scripted aggregate explicitly.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        grants.clone(),
        scope_type.clone(),
    ));
    let evaluator = evaluator(rbac);

    let request = EvaluationRequestBuilder::default().build();
    match evaluator.evaluate_permissions(&request).await.unwrap() {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(granted.grants.len(), 2);
            assert_eq!(granted.scope_type, scope_type);
        }
        PolicyOutcome::Denied(_) => panic!("expected Allowed"),
    }
}

#[tokio::test]
async fn rbac_request_carries_stringified_subject_id() {
    let subject_id = Uuid::new_v4();
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));

    let request = EvaluationRequestBuilder::default()
        .with_subject_id(subject_id)
        .build();
    evaluator.evaluate_permissions(&request).await.unwrap();

    let last = rbac
        .last_evaluate_permission_request()
        .expect("RBAC was called");
    assert_eq!(last.subject_id, subject_id.to_string());
}

#[tokio::test]
async fn rbac_request_carries_translated_scope() {
    let tenant_id = Uuid::new_v4();
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));

    let request = EvaluationRequestBuilder::default()
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: Some(tenant_id),
            barrier_mode: BarrierMode::Respect,
            tenant_status: None,
        }))
        .build();
    evaluator.evaluate_permissions(&request).await.unwrap();

    let last = rbac.last_evaluate_permission_request().unwrap();
    assert_eq!(last.context_scope, RbacScope::tenant(tenant_id));
}

#[tokio::test]
async fn rbac_request_carries_action_and_resource_type() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));

    let request = EvaluationRequestBuilder::default()
        .with_action_name("update")
        .with_resource_type("gts.cf.resources.compute.vm.v1~")
        .build();
    evaluator.evaluate_permissions(&request).await.unwrap();

    let last = rbac.last_evaluate_permission_request().unwrap();
    assert_eq!(last.operation, "update");
    assert_eq!(last.resource_type, "gts.cf.resources.compute.vm.v1~");
}

#[tokio::test]
async fn user_principal_type_passed_to_rbac() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some(SUBJECT_TYPE_USER.to_owned()))
        .build();
    evaluator.evaluate_permissions(&request).await.unwrap();
    let last = rbac.last_evaluate_permission_request().unwrap();
    assert_eq!(last.principal_type, PrincipalType::User);
}

#[tokio::test]
async fn service_principal_type_passed_to_rbac() {
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_subject_type(Some(SUBJECT_TYPE_SERVICE_PRINCIPAL.to_owned()))
        .build();
    evaluator.evaluate_permissions(&request).await.unwrap();
    let last = rbac.last_evaluate_permission_request().unwrap();
    assert_eq!(last.principal_type, PrincipalType::ServicePrincipal);
}

// ------------ #3: operation canonicalization ------------

#[test]
fn canonicalize_operation_maps_read_aliases_to_read() {
    assert_eq!(canonicalize_operation("get"), "read");
    assert_eq!(canonicalize_operation("list"), "read");
}

#[test]
fn canonicalize_operation_passes_through_canonical_verbs() {
    for verb in [
        "read", "write", "delete", "start", "stop", "restart", "create", "update",
    ] {
        assert_eq!(canonicalize_operation(verb), verb);
    }
}

#[tokio::test]
async fn get_action_reaches_rbac_as_read() {
    // RBAC matches the operation by exact canonical verb; a read-only role
    // grants `read`, so a `get` request must arrive at RBAC as `read`.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_action_name("get")
        .build();
    evaluator
        .evaluate_permissions(&request)
        .await
        .expect("allow");
    let last = rbac
        .last_evaluate_permission_request()
        .expect("rbac called");
    assert_eq!(
        last.operation, "read",
        "get must be canonicalized to read for RBAC"
    );
}

// ------------ #2: S2S caller context ------------

#[test]
fn build_rbac_ctx_presents_first_party_root_with_subject_home_tenant() {
    let tenant = Uuid::from_u128(0xCAFE);
    let subject = Uuid::from_u128(0x111);
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(subject)
        .with_subject_tenant_id(tenant)
        .build();
    let ctx = build_rbac_ctx(&request).expect("ctx built");
    // First-party Root for the RBAC caller-gate.
    assert_eq!(ctx.token_scopes().len(), 1);
    assert_eq!(ctx.token_scopes()[0], "*");
    // Eval-tenant fallback (Scope::Root) must be the subject's home tenant.
    assert_eq!(ctx.subject_tenant_id(), tenant);
    assert_eq!(ctx.subject_id(), subject);
}

#[test]
fn build_rbac_ctx_falls_back_to_request_root_id_when_subject_tenant_absent() {
    let root = Uuid::from_u128(0xBEEF);
    let request = EvaluationRequestBuilder::default()
        .without_subject_tenant()
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: Some(root),
            barrier_mode: BarrierMode::Respect,
            tenant_status: None,
        }))
        .build();
    let ctx = build_rbac_ctx(&request).expect("ctx built from root_id fallback");
    assert_eq!(ctx.subject_tenant_id(), root);
    assert_eq!(ctx.token_scopes()[0], "*");
}

#[test]
fn build_rbac_ctx_fails_closed_when_no_tenant_resolvable() {
    let request = EvaluationRequestBuilder::default()
        .without_subject_tenant()
        .build();
    match build_rbac_ctx(&request) {
        Err(err @ PluginError::Internal { .. }) => {
            let msg = err.to_string();
            assert!(msg.contains("RBAC caller context"), "got {msg:?}");
        }
        other => panic!("expected Internal fail-closed, got {other:?}"),
    }
}

// ------------ #4: AuthorizationDenied → business deny ------------

#[tokio::test]
async fn rbac_authorization_denied_maps_to_business_deny_not_503() {
    // A 403-class authorization failure must surface as a deny, NOT a phantom
    // 503 ServiceUnavailable.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::authorization_denied("caller scope mismatch"),
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default().build();
    match evaluator.evaluate_permissions(&request).await {
        Ok(PolicyOutcome::Denied(response)) => {
            assert!(!response.decision);
            assert_eq!(
                response.context.deny_reason.expect("populated").error_code,
                INSUFFICIENT_PERMISSIONS_V1
            );
        }
        other => panic!("AuthorizationDenied must map to a business deny, got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 1);
}

// ------------ Trusted system-actor scope grant ------------

/// The unforgeable AM system actor with a nil home tenant (the
/// platform-scoped `build_inner(None)` shape — the PEP forwards the
/// nil verbatim in `properties["tenant_id"]`) must receive a Global
/// grant: a subtree rooted at the nil sentinel materializes to zero
/// tenants and fails the caller closed.
#[tokio::test]
async fn trusted_system_actor_nil_home_tenant_grants_global() {
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(TEST_AM_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some("am.system".to_owned()))
        .with_subject_tenant_id(Uuid::nil())
        .build();
    let outcome = evaluator(Arc::clone(&rbac))
        .evaluate_permissions(&request)
        .await
        .expect("trusted actor must short-circuit to Allow");
    match outcome {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(granted.scope_type, PermissionScopeType::Global);
        }
        other @ PolicyOutcome::Denied(_) => panic!("expected Allowed(Global), got {other:?}"),
    }
    assert_eq!(rbac.call_count(), 0, "short-circuit must never reach RBAC");
}

/// A live (non-nil) home tenant keeps the narrower subtree grant —
/// the Global widening applies ONLY to the platform-scoped shape.
#[tokio::test]
async fn trusted_system_actor_live_home_tenant_keeps_subtree_grant() {
    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let home = Uuid::from_u128(0xA11CE);
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(TEST_AM_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some("am.system".to_owned()))
        .with_subject_tenant_id(home)
        .build();
    let outcome = evaluator(Arc::clone(&rbac))
        .evaluate_permissions(&request)
        .await
        .expect("trusted actor must short-circuit to Allow");
    match outcome {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(
                granted.scope_type,
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: home
                }
            );
        }
        other @ PolicyOutcome::Denied(_) => {
            panic!("expected Allowed(TenantSubtree), got {other:?}")
        }
    }
}

// ------------ Trusted system-actor short-circuit (rms.system) ------------

#[tokio::test]
async fn rms_system_actor_short_circuits_to_tenant_clamped_allow() {
    // A tenant-bound rms.system request is Allowed clamped to the subject's
    // home tenant — RBAC is never consulted (the client is wired to error, so a
    // non-zero call count would prove the short-circuit failed to fire).
    let tenant = Uuid::from_u128(0x7777);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::authorization_denied("must not be called"),
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(TEST_RMS_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some(TEST_RMS_SYSTEM_SUBJECT_TYPE.to_owned()))
        .with_subject_tenant_id(tenant)
        .build();
    match evaluator.evaluate_permissions(&request).await.unwrap() {
        PolicyOutcome::Allowed(granted) => {
            assert!(granted.grants.is_empty());
            assert_eq!(
                granted.scope_type,
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant
                }
            );
        }
        PolicyOutcome::Denied(_) => panic!("expected Allowed for rms.system actor"),
    }
    assert_eq!(rbac.call_count(), 0, "short-circuit must not call RBAC");
}

#[tokio::test]
async fn rms_system_actor_without_home_tenant_clamps_to_global() {
    // A platform-scoped rms.system request (no home tenant) is Allowed at Global.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::authorization_denied("must not be called"),
    ));
    let evaluator = evaluator(Arc::clone(&rbac));
    let request = EvaluationRequestBuilder::default()
        .with_subject_id(TEST_RMS_SYSTEM_ACTOR_UUID)
        .with_subject_type(Some(TEST_RMS_SYSTEM_SUBJECT_TYPE.to_owned()))
        .without_subject_tenant()
        .build();
    match evaluator.evaluate_permissions(&request).await.unwrap() {
        PolicyOutcome::Allowed(granted) => {
            assert_eq!(granted.scope_type, PermissionScopeType::Global);
        }
        PolicyOutcome::Denied(_) => panic!("expected Allowed for rms.system actor"),
    }
    assert_eq!(rbac.call_count(), 0, "short-circuit must not call RBAC");
}

/// A `tenant_id` claim that is present but unreadable MUST fail closed.
///
/// `build_rbac_ctx` falls back to `tenant_context.root_id` when the claim is
/// **absent** — that is documented and intended. Collapsing an unparseable
/// value into the same `None` sent it down that fallback too, so the RBAC
/// caller context was built with the *root* tenant as the subject's home
/// tenant. RBAC uses that as its `Scope::Root` eval-tenant and as the
/// group-membership fallback, so a malformed claim silently widened the
/// subject's evaluation context instead of rejecting the request.
#[test]
fn malformed_subject_tenant_id_is_rejected_not_treated_as_absent() {
    let request = EvaluationRequestBuilder::default()
        .with_raw_subject_tenant("not-a-uuid")
        .build();

    let err = build_rbac_ctx(&request)
        .expect_err("an unreadable tenant_id claim MUST NOT build a caller context");

    match err {
        err @ PluginError::UnreadableSubjectTenant { .. } => {
            let msg = err.to_string();
            assert!(
                msg.contains("tenant_id"),
                "the diagnostic should name the offending claim, got: {msg}"
            );
        }
        other => panic!("expected UnreadableSubjectTenant, got {other:?}"),
    }
}

/// The counterpart the fix must not break: an **absent** claim still takes
/// the documented `tenant_context.root_id` fallback.
#[test]
fn absent_subject_tenant_id_still_falls_back_to_the_root_tenant() {
    let root = Uuid::now_v7();
    let request = EvaluationRequestBuilder::default()
        .without_subject_tenant()
        .with_tenant_context(Some(TenantContext {
            mode: TenantMode::Subtree,
            root_id: Some(root),
            barrier_mode: BarrierMode::Respect,
            tenant_status: None,
        }))
        .build();

    let ctx = build_rbac_ctx(&request)
        .expect("an absent claim must still resolve through tenant_context.root_id");
    assert_eq!(
        ctx.subject_tenant_id(),
        root,
        "the absent-claim fallback must use tenant_context.root_id"
    );
}
