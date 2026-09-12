// Created: 2026-04-14 by Constructor Tech
use async_trait::async_trait;

use super::*;
use crate::constraints::{Constraint, InGroupPredicate, InPredicate, Predicate};
use crate::models::{EvaluationResponse, EvaluationResponseContext};
use toolkit_gts::gts_id;
use toolkit_security::{PlatformIdentity, PlatformSecurityContext, pep_properties};

fn uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).expect("valid test UUID")
}

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const SUBJECT: &str = "22222222-2222-2222-2222-222222222222";
const RESOURCE: &str = "33333333-3333-3333-3333-333333333333";

/// Mock that mirrors the real `static-authz-plugin` behaviour:
/// 1. Use `tenant_context.root_id` if present (explicit PEP override).
/// 2. Fall back to `subject.properties["tenant_id"]` (PDP self-resolution).
/// 3. Nil UUID → deny rather than grant unrestricted access.
/// 4. No tenant at all → deny.
struct AllowAllMock;

#[async_trait]
impl AuthZResolverApi for AllowAllMock {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        // Resolve tenant: explicit context first, then subject fallback.
        let tenant_id = req
            .context
            .tenant_context
            .as_ref()
            .and_then(|tc| tc.root_id)
            .or_else(|| {
                req.subject
                    .properties
                    .get("tenant_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });

        let Some(tid) = tenant_id else {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        };

        if tid == Uuid::default() {
            return Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            });
        }

        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tid],
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// Mock that always returns `decision=false` with an optional deny reason.
struct DenyMock {
    deny_reason: Option<crate::models::DenyReason>,
}

impl DenyMock {
    fn new() -> Self {
        Self { deny_reason: None }
    }

    fn with_reason(error_code: &str, details: Option<&str>) -> Self {
        Self {
            deny_reason: Some(crate::models::DenyReason {
                error_code: error_code.to_owned(),
                details: details.map(ToOwned::to_owned),
            }),
        }
    }
}

#[async_trait]
impl AuthZResolverApi for DenyMock {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext {
                deny_reason: self.deny_reason.clone(),
                ..Default::default()
            },
        })
    }
}

/// Mock that always returns an RPC error.
struct FailMock;

#[async_trait]
impl AuthZResolverApi for FailMock {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::internal("boom").create())
    }
}

/// Mock that never responds in time — models a hung/slow PDP.
struct SlowMock;

#[async_trait]
impl AuthZResolverApi for SlowMock {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        std::future::pending::<()>().await;
        unreachable!("SlowMock should never complete before the PEP deadline");
    }
}

/// Shared slot holding the last `(ctx, request)` the PEP passed to `evaluate`.
type CapturedCall = Arc<std::sync::Mutex<Option<(PlatformSecurityContext, EvaluationRequest)>>>;

/// Mock that captures the arguments the PEP passes to `evaluate`, so tests can
/// assert the caller identity actually reaches the resolver. Returns an
/// allow-all decision constrained to the request's resolved tenant.
#[derive(Clone)]
struct CapturingMock {
    captured: CapturedCall,
}

impl CapturingMock {
    fn new() -> Self {
        Self {
            captured: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

#[async_trait]
impl AuthZResolverApi for CapturingMock {
    async fn evaluate(
        &self,
        ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant = req
            .subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_default();
        *self.captured.lock().unwrap() = Some((ctx, req));
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        [tenant],
                    ))],
                }],
                ..Default::default()
            },
        })
    }
}

/// Mock PDP response used to verify that the resource descriptor's trusted RG
/// member-handle type reaches the compiled `AccessScope`.
struct GroupScopeMock;

#[async_trait]
impl AuthZResolverApi for GroupScopeMock {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![
                        Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [uuid(TENANT)],
                        )),
                        Predicate::InGroup(InGroupPredicate::new(
                            pep_properties::RESOURCE_ID,
                            [uuid(RESOURCE)],
                        )),
                    ],
                }],
                ..Default::default()
            },
        })
    }
}

fn test_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid(SUBJECT))
        .subject_tenant_id(uuid(TENANT))
        .build()
        .unwrap()
}

const TEST_RESOURCE: ResourceType = ResourceType::from_static(
    gts_id!("cf.core.users.user.v1~"),
    &[pep_properties::OWNER_TENANT_ID, pep_properties::RESOURCE_ID],
);
const GROUP_MEMBERSHIP_TYPE: &str = gts_id!("cf.core.rg.type.v1~cf.core.users.user.v1~");
const GROUP_TYPED_TEST_RESOURCE: ResourceType =
    TEST_RESOURCE.with_group_membership_type(GROUP_MEMBERSHIP_TYPE);

fn enforcer(mock: impl AuthZResolverApi + 'static) -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(mock))
}

// ── from_hub (lazy resolution) ───────────────────────────────────

#[tokio::test]
async fn from_hub_resolves_registered_client() {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn AuthZResolverApi>(Arc::new(AllowAllMock));
    let e = PolicyEnforcer::from_hub(hub);
    let ctx = test_ctx();

    let scope = e
        .access_scope(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)))
        .await
        .expect("lazy resolution should succeed once the client is registered");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(TENANT)]
    );
}

#[tokio::test]
async fn from_hub_errors_when_client_unregistered() {
    let hub = Arc::new(ClientHub::new());
    let e = PolicyEnforcer::from_hub(hub);
    let ctx = test_ctx();

    let err = e
        .access_scope(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)))
        .await
        .expect_err("an unregistered client should surface a transport error");

    assert!(matches!(err, EnforcerError::EvaluationFailed(_)));
}

// ── evaluate deadline (fail-fast on a slow PDP) ──────────────────

#[tokio::test]
async fn evaluate_times_out_when_pdp_is_slow() {
    let e = enforcer(SlowMock).with_deadline(std::time::Duration::from_millis(20));
    let ctx = test_ctx();

    let start = std::time::Instant::now();
    let err = e
        .access_scope(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)))
        .await
        .expect_err("a PDP slower than the deadline must fail fast");

    assert!(matches!(err, EnforcerError::EvaluationFailed(_)));
    assert!(
        start.elapsed() < std::time::Duration::from_secs(1),
        "the deadline must bound the call to well under the PDP's response time"
    );
}

// ── build_request ────────────────────────────────────────────────

#[test]
fn build_request_populates_fields() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let req = e.build_request(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)), true);

    assert_eq!(
        req.resource.resource_type,
        gts_id!("cf.core.users.user.v1~")
    );
    assert_eq!(req.action.name, "get");
    assert_eq!(req.resource.id, Some(uuid(RESOURCE)));
    assert!(req.context.require_constraints);
    // No explicit context_tenant_id → tenant_context is None (PDP decides)
    assert!(req.context.tenant_context.is_none());
}

#[test]
fn build_request_suppresses_group_capabilities_for_untyped_resource() {
    assert_eq!(
        TEST_RESOURCE
            .with_group_membership_type("")
            .group_membership_type(),
        None,
        "an empty mapping must normalize to an untyped resource"
    );
    let e = enforcer(AllowAllMock).with_capabilities(vec![
        Capability::TenantHierarchy,
        Capability::GroupMembership,
        Capability::GroupHierarchy,
    ]);

    let req = e.build_request(&test_ctx(), &TEST_RESOURCE, "list", None, true);

    assert_eq!(req.context.capabilities, vec![Capability::TenantHierarchy]);
}

#[test]
fn build_request_preserves_group_capabilities_for_typed_resource() {
    let capabilities = vec![Capability::GroupMembership, Capability::GroupHierarchy];
    let e = enforcer(AllowAllMock).with_capabilities(capabilities.clone());

    let req = e.build_request(&test_ctx(), &GROUP_TYPED_TEST_RESOURCE, "list", None, true);

    assert_eq!(req.context.capabilities, capabilities);
    assert_eq!(
        GROUP_TYPED_TEST_RESOURCE.group_membership_type(),
        Some(GROUP_MEMBERSHIP_TYPE)
    );
}

#[test]
fn build_request_suppresses_group_hierarchy_without_group_membership() {
    // `InGroupSubtree` compilation requires BOTH group capabilities, so a
    // lone `GroupHierarchy` advertisement is a dead capability the PDP could
    // only act on with predicates destined to fail — it must be suppressed.
    let e = enforcer(AllowAllMock).with_capabilities(vec![
        Capability::TenantHierarchy,
        Capability::GroupHierarchy,
    ]);

    let req = e.build_request(&test_ctx(), &GROUP_TYPED_TEST_RESOURCE, "list", None, true);

    assert_eq!(req.context.capabilities, vec![Capability::TenantHierarchy]);
}

#[test]
fn build_request_suppresses_group_capabilities_without_resource_id_property() {
    // Native group predicates are hard-restricted to the `id` property; a
    // typed resource that does not support `id` can never execute one, so
    // its group capabilities must be suppressed despite the mapping.
    const NO_ID_GROUP_TYPED_RESOURCE: ResourceType = ResourceType::from_static(
        gts_id!("cf.core.users.user.v1~"),
        &[pep_properties::OWNER_TENANT_ID],
    )
    .with_group_membership_type(GROUP_MEMBERSHIP_TYPE);

    let e = enforcer(AllowAllMock).with_capabilities(vec![
        Capability::TenantHierarchy,
        Capability::GroupMembership,
        Capability::GroupHierarchy,
    ]);

    let req = e.build_request(&test_ctx(), &NO_ID_GROUP_TYPED_RESOURCE, "list", None, true);

    assert_eq!(req.context.capabilities, vec![Capability::TenantHierarchy]);
}

#[test]
fn build_request_with_overrides_tenant() {
    let custom_tenant = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let req = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "list",
        None,
        false,
        &AccessRequest::new().context_tenant_id(custom_tenant),
    );

    assert_eq!(
        req.context
            .tenant_context
            .as_ref()
            .and_then(|tc| tc.root_id),
        Some(custom_tenant),
    );
    assert!(!req.context.require_constraints);
}

// ── access_scope ─────────────────────────────────────────────────

#[tokio::test]
async fn access_scope_threads_caller_identity_into_subject() {
    // The core behaviour: the caller's SecurityContext must reach the resolver.
    // Post two-plane-auth it travels in `req.subject` (+ context), while the
    // `PlatformSecurityContext` argument is a credential-free plane marker.
    let mock = CapturingMock::new();
    let captured = Arc::clone(&mock.captured);
    let e = enforcer(mock);
    let ctx = SecurityContext::builder()
        .subject_id(uuid(SUBJECT))
        .subject_tenant_id(uuid(TENANT))
        .token_scopes(vec!["users:read".to_owned()])
        .bearer_token("caller-jwt".to_owned())
        .build()
        .unwrap();

    e.access_scope(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)))
        .await
        .expect("allow-all mock should succeed");

    let (plane_ctx, req) = captured
        .lock()
        .unwrap()
        .take()
        .expect("evaluate must have been called");

    // Caller identity is threaded via the request, not the ctx argument.
    assert_eq!(req.subject.id, uuid(SUBJECT));
    assert_eq!(
        req.subject
            .properties
            .get("tenant_id")
            .and_then(|v| v.as_str()),
        Some(TENANT),
    );
    assert_eq!(req.context.token_scopes, vec!["users:read".to_owned()]);
    assert!(
        req.context.bearer_token.is_some(),
        "caller bearer token must reach the PDP"
    );

    // The ctx argument is a credential-free plane marker carrying no identity.
    assert!(matches!(plane_ctx.identity(), PlatformIdentity::Unknown));
}

#[tokio::test]
async fn access_scope_carries_resource_group_membership_type_into_filter() {
    let scope = enforcer(GroupScopeMock)
        .with_capabilities(vec![Capability::GroupMembership])
        .access_scope(&test_ctx(), &GROUP_TYPED_TEST_RESOURCE, "list", None)
        .await
        .expect("typed resource must compile the negotiated native group predicate");

    let filter = &scope.constraints()[0].filters()[1];
    let toolkit_security::ScopeFilter::InGroup(filter) = filter else {
        panic!("expected InGroup filter, got {filter:?}");
    };
    assert_eq!(filter.membership_resource_type(), GROUP_MEMBERSHIP_TYPE);
}

#[tokio::test]
async fn access_scope_rejects_unadvertised_native_group_predicate() {
    let error = enforcer(GroupScopeMock)
        .access_scope(&test_ctx(), &GROUP_TYPED_TEST_RESOURCE, "list", None)
        .await
        .expect_err("a native group predicate must have been advertised");

    let EnforcerError::CompileFailed(ConstraintCompileError::UnadvertisedCapabilities {
        predicate,
        missing,
    }) = error
    else {
        panic!("expected typed unadvertised-capability error, got: {error:?}");
    };
    assert_eq!(predicate, "InGroup");
    assert_eq!(missing, vec!["group_membership"]);
}

#[tokio::test]
async fn access_scope_rejects_group_predicate_after_suppressing_untyped_capability() {
    let error = enforcer(GroupScopeMock)
        .with_capabilities(vec![Capability::GroupMembership])
        .access_scope(&test_ctx(), &TEST_RESOURCE, "list", None)
        .await
        .expect_err("an untyped resource must not accept an unsolicited group predicate");

    let EnforcerError::CompileFailed(ConstraintCompileError::UnadvertisedCapabilities {
        predicate,
        missing,
    }) = error
    else {
        panic!(
            "the resource mapping must suppress the configured capability before evaluation, \
             got: {error:?}"
        );
    };
    assert_eq!(predicate, "InGroup");
    assert_eq!(missing, vec!["group_membership"]);
}

#[tokio::test]
async fn access_scope_pdp_resolves_tenant_from_subject() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    // access_scope() passes default() — PDP resolves tenant from
    // subject.properties["tenant_id"] via its own fallback logic.
    let scope = e
        .access_scope(&ctx, &TEST_RESOURCE, "get", Some(uuid(RESOURCE)))
        .await
        .expect("PDP should resolve tenant from subject properties");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(TENANT)]
    );
}

#[tokio::test]
async fn access_scope_with_explicit_tenant_returns_scope() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let scope = e
        .access_scope_with(
            &ctx,
            &TEST_RESOURCE,
            "get",
            Some(uuid(RESOURCE)),
            &AccessRequest::new().context_tenant_id(uuid(TENANT)),
        )
        .await
        .expect("should succeed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(TENANT)]
    );
}

#[tokio::test]
async fn access_scope_with_for_create() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let scope = e
        .access_scope_with(
            &ctx,
            &TEST_RESOURCE,
            "create",
            None,
            &AccessRequest::new()
                .context_tenant_id(uuid(TENANT))
                .tenant_mode(TenantMode::RootOnly),
        )
        .await
        .expect("should succeed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(TENANT)]
    );
}

#[tokio::test]
async fn access_scope_denied_returns_denied_error() {
    let e = enforcer(DenyMock::new());
    let ctx = test_ctx();
    let result = e.access_scope(&ctx, &TEST_RESOURCE, "get", None).await;

    assert!(matches!(
        result,
        Err(EnforcerError::Denied { deny_reason: None })
    ));
}

#[tokio::test]
async fn access_scope_denied_with_reason() {
    let e = enforcer(DenyMock::with_reason(
        "INSUFFICIENT_PERMISSIONS",
        Some("Missing admin role"),
    ));
    let ctx = test_ctx();
    let result = e.access_scope(&ctx, &TEST_RESOURCE, "get", None).await;

    match result {
        Err(EnforcerError::Denied { deny_reason }) => {
            let reason = deny_reason.expect("should have deny_reason");
            assert_eq!(reason.error_code, "INSUFFICIENT_PERMISSIONS");
            assert_eq!(reason.details.as_deref(), Some("Missing admin role"));
        }
        other => panic!("Expected Denied with reason, got: {other:?}"),
    }
}

#[tokio::test]
async fn access_scope_evaluation_failure() {
    let e = enforcer(FailMock);
    let ctx = test_ctx();
    let result = e.access_scope(&ctx, &TEST_RESOURCE, "get", None).await;

    assert!(matches!(result, Err(EnforcerError::EvaluationFailed(_))));
}

#[tokio::test]
async fn access_scope_anonymous_denied_by_pdp() {
    let e = enforcer(AllowAllMock);
    let ctx = SecurityContext::anonymous();
    // anonymous() has nil UUID as subject_tenant_id → PDP resolves it
    // from subject.properties["tenant_id"], sees nil UUID, and denies.
    let result = e.access_scope(&ctx, &TEST_RESOURCE, "list", None).await;

    assert!(matches!(
        result,
        Err(EnforcerError::Denied { deny_reason: None })
    ));
}

// ── builder methods ──────────────────────────────────────────────

#[test]
fn with_capabilities() {
    let e = enforcer(AllowAllMock).with_capabilities(vec![Capability::TenantHierarchy]);

    assert_eq!(e.capabilities, vec![Capability::TenantHierarchy]);
}

#[test]
fn debug_impl() {
    let e = enforcer(AllowAllMock);
    let dbg = format!("{e:?}");
    assert!(dbg.contains("PolicyEnforcer"));
}

// ── AccessRequest builder ─────────────────────────────────────────

#[test]
fn access_request_default_is_empty() {
    let req = AccessRequest::new();
    assert!(req.resource_properties.is_empty());
    assert!(req.tenant_context.is_none());
}

#[test]
fn access_request_builder_chain() {
    let tid = uuid(TENANT);
    let req = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, tid)
        .context_tenant_id(tid)
        .tenant_mode(TenantMode::RootOnly)
        .barrier_mode(BarrierMode::Ignore)
        .tenant_status(vec!["active".to_owned()]);

    assert_eq!(req.resource_properties.len(), 1);
    let tc = req.tenant_context.as_ref().expect("tenant context");
    assert_eq!(tc.root_id, Some(tid));
    assert_eq!(tc.mode, TenantMode::RootOnly);
    assert_eq!(tc.barrier_mode, BarrierMode::Ignore);
    assert_eq!(tc.tenant_status, Some(vec!["active".to_owned()]));
}

#[test]
fn access_request_tenant_context_setter() {
    let tid = uuid(TENANT);
    let req = AccessRequest::new().tenant_context(TenantContext {
        mode: TenantMode::RootOnly,
        root_id: Some(tid),
        ..Default::default()
    });

    let tc = req.tenant_context.as_ref().expect("tenant context");
    assert_eq!(tc.root_id, Some(tid));
    assert_eq!(tc.mode, TenantMode::RootOnly);
    assert_eq!(tc.barrier_mode, BarrierMode::Respect);
}

#[test]
fn access_request_resource_properties_replaces() {
    let mut props = HashMap::new();
    props.insert("a".to_owned(), serde_json::json!("1"));
    props.insert("b".to_owned(), serde_json::json!("2"));

    let req = AccessRequest::new()
        .resource_property("old_key", serde_json::json!("old"))
        .resource_properties(props);

    assert_eq!(req.resource_properties.len(), 2);
    assert!(!req.resource_properties.contains_key("old_key"));
}

#[test]
fn into_property_value_implementations() {
    let uuid_val = uuid(TENANT);
    let req = AccessRequest::new()
        .resource_property("uuid_prop", uuid_val)
        .resource_property("uuid_ref_prop", uuid_val)
        .resource_property("string_prop", "test".to_owned())
        .resource_property("str_prop", "test")
        .resource_property("int_prop", 42i64)
        .resource_property("json_prop", serde_json::json!({"key": "value"}));

    assert_eq!(req.resource_properties.len(), 6);
    assert_eq!(
        req.resource_properties.get("uuid_prop"),
        Some(&serde_json::json!(uuid_val.to_string())),
    );
    assert_eq!(
        req.resource_properties.get("string_prop"),
        Some(&serde_json::json!("test")),
    );
    assert_eq!(
        req.resource_properties.get("int_prop"),
        Some(&serde_json::json!(42)),
    );
}

// ── build_request_with ────────────────────────────────────────────

#[test]
fn build_request_with_applies_resource_properties() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let tid = uuid(TENANT);
    let req = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "create",
        None,
        false,
        &AccessRequest::new().resource_property(pep_properties::OWNER_TENANT_ID, tid),
    );

    assert_eq!(
        req.resource.properties.get(pep_properties::OWNER_TENANT_ID),
        Some(&serde_json::json!(tid.to_string())),
    );
}

#[test]
fn build_request_with_applies_tenant_mode_and_barrier() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let req = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "list",
        None,
        true,
        &AccessRequest::new()
            .tenant_mode(TenantMode::RootOnly)
            .barrier_mode(BarrierMode::Ignore)
            .tenant_status(vec!["active".to_owned()]),
    );

    let tc = req.context.tenant_context.as_ref().expect("tenant context");
    assert_eq!(tc.mode, TenantMode::RootOnly);
    assert_eq!(tc.barrier_mode, BarrierMode::Ignore);
    assert_eq!(tc.tenant_status, Some(vec!["active".to_owned()]));
}

#[test]
fn build_request_with_default_has_no_tenant_context() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let req = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "get",
        None,
        true,
        &AccessRequest::default(),
    );

    // No explicit context_tenant_id → tenant_context is None (PDP decides)
    assert!(req.context.tenant_context.is_none());
}

// ── access_scope_with ─────────────────────────────────────────────

#[tokio::test]
async fn access_scope_with_custom_tenant() {
    let custom_tenant = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let scope = e
        .access_scope_with(
            &ctx,
            &TEST_RESOURCE,
            "list",
            None,
            &AccessRequest::new().context_tenant_id(custom_tenant),
        )
        .await
        .expect("should succeed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[custom_tenant]
    );
}

#[tokio::test]
async fn access_scope_with_resource_properties() {
    let e = enforcer(AllowAllMock);
    let ctx = test_ctx();
    let scope = e
        .access_scope_with(
            &ctx,
            &TEST_RESOURCE,
            "get",
            None,
            &AccessRequest::new()
                .resource_property(
                    pep_properties::OWNER_TENANT_ID,
                    serde_json::json!(uuid(TENANT).to_string()),
                )
                .context_tenant_id(uuid(TENANT))
                .tenant_mode(TenantMode::RootOnly),
        )
        .await
        .expect("should succeed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        &[uuid(TENANT)]
    );
}

// ── request builder internals ────────────────────────────────────

#[test]
fn builds_request_with_all_fields() {
    const USERS_RESOURCE: ResourceType = ResourceType::from_static(
        gts_id!("cf.core.users.user.v1~"),
        &[pep_properties::OWNER_TENANT_ID],
    );

    let context_tenant_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let subject_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
    let subject_tenant_id = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
    let resource_id = Uuid::parse_str("44444444-4444-4444-4444-444444444444").unwrap();

    let ctx = SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(subject_tenant_id)
        .subject_type("user")
        .token_scopes(vec!["admin".to_owned()])
        .bearer_token("test-token".to_owned())
        .build()
        .unwrap();

    let e = PolicyEnforcer::new(Arc::new(AllowAllMock))
        .with_capabilities(vec![Capability::TenantHierarchy]);

    let access_req = AccessRequest::new().tenant_context(TenantContext {
        root_id: Some(context_tenant_id),
        ..Default::default()
    });

    let request = e.build_request_with(
        &ctx,
        &USERS_RESOURCE,
        "get",
        Some(resource_id),
        true,
        &access_req,
    );

    assert_eq!(request.subject.id, subject_id);
    assert_eq!(
        request.subject.properties.get("tenant_id").unwrap(),
        &serde_json::Value::String(subject_tenant_id.to_string())
    );
    assert_eq!(request.subject.subject_type.as_deref(), Some("user"));
    assert_eq!(request.action.name, "get");
    assert_eq!(
        request.resource.resource_type,
        gts_id!("cf.core.users.user.v1~")
    );
    assert_eq!(request.resource.id, Some(resource_id));
    assert!(request.context.require_constraints);
    assert_eq!(
        request.context.tenant_context.as_ref().unwrap().root_id,
        Some(context_tenant_id)
    );
    assert_eq!(request.context.token_scopes, vec!["admin"]);
    assert_eq!(
        request.context.capabilities,
        vec![Capability::TenantHierarchy]
    );
    assert!(request.context.bearer_token.is_some());
    assert_eq!(
        request.context.supported_properties,
        vec![pep_properties::OWNER_TENANT_ID]
    );
}

#[test]
fn builds_request_without_tenant_context() {
    let ctx = SecurityContext::anonymous();

    let e = enforcer(AllowAllMock);

    let request = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "create",
        None,
        false,
        &AccessRequest::default(),
    );

    // No explicit context_tenant_id → tenant_context is None (PDP decides)
    assert!(request.context.tenant_context.is_none());
    assert!(!request.context.require_constraints);
    assert_eq!(request.resource.id, None);
    assert!(request.context.capabilities.is_empty());
    assert!(request.context.bearer_token.is_none());
}

#[test]
fn applies_resource_properties() {
    let tenant_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap();

    let e = enforcer(AllowAllMock);
    let access_req = AccessRequest::new()
        .resource_property(
            pep_properties::OWNER_TENANT_ID,
            serde_json::Value::String(tenant_id.to_string()),
        )
        .context_tenant_id(tenant_id);

    let request = e.build_request_with(&ctx, &TEST_RESOURCE, "create", None, false, &access_req);

    assert_eq!(
        request
            .resource
            .properties
            .get(pep_properties::OWNER_TENANT_ID),
        Some(&serde_json::Value::String(tenant_id.to_string())),
    );
}

#[test]
fn applies_tenant_mode_and_barrier_mode() {
    let tenant_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap();

    let e = enforcer(AllowAllMock);
    let access_req = AccessRequest::new().tenant_context(TenantContext {
        mode: TenantMode::RootOnly,
        root_id: Some(tenant_id),
        barrier_mode: BarrierMode::Ignore,
        tenant_status: Some(vec!["active".to_owned()]),
    });

    let request = e.build_request_with(&ctx, &TEST_RESOURCE, "list", None, true, &access_req);

    let tc = request.context.tenant_context.as_ref().unwrap();
    assert_eq!(tc.mode, TenantMode::RootOnly);
    assert_eq!(tc.barrier_mode, BarrierMode::Ignore);
    assert_eq!(tc.tenant_status, Some(vec!["active".to_owned()]));
}

#[test]
fn default_request_has_no_tenant_context_pdp_decides() {
    let subject_tenant = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
        .subject_tenant_id(subject_tenant)
        .build()
        .unwrap();

    let e = enforcer(AllowAllMock);

    // No tenant_context provided — PEP passes None, PDP decides
    let request = e.build_request_with(
        &ctx,
        &TEST_RESOURCE,
        "list",
        None,
        true,
        &AccessRequest::default(),
    );

    // tenant_context is None — PDP will resolve from subject.properties
    assert!(request.context.tenant_context.is_none());
    // But subject.properties["tenant_id"] is still populated for the PDP
    assert_eq!(
        request.subject.properties.get("tenant_id"),
        Some(&serde_json::json!(subject_tenant.to_string())),
    );
}

#[test]
fn explicit_root_id_overrides_subject_tenant() {
    let subject_tenant = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let explicit_tenant = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
    let ctx = SecurityContext::builder()
        .subject_id(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
        .subject_tenant_id(subject_tenant)
        .build()
        .unwrap();

    let e = enforcer(AllowAllMock);
    let access_req = AccessRequest::new().context_tenant_id(explicit_tenant);

    let request = e.build_request_with(&ctx, &TEST_RESOURCE, "get", None, true, &access_req);

    let tc = request.context.tenant_context.as_ref().unwrap();
    assert_eq!(tc.root_id, Some(explicit_tenant));
}
