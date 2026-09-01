#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::inconsistent_struct_constructor
)]
//! End-to-end tests for orchestration + audit emission.
//!
//! Captures `tracing` events for `target = "cf-authz.audit"` via a custom
//! `Layer` registered into `tracing_subscriber::Registry`. Each test scopes
//! a fresh subscriber so emissions are isolated.
//!
//! Coverage: audit on/off gating, allow vs deny field shape, Err-skip-audit,
//! `correlation_id` uniqueness, bearer-token absence, raw-predicate absence,
//! `require_constraints` branching, empty-materialization denial, provenance
//! rejection auditing, GTS Strict-mode reclassification, and pipeline
//! short-circuit ordering.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use authz_resolver_plugin::AuthZResolverPluginGear;
use authz_resolver_plugin::test_support::EvaluationRequestBuilder;
use authz_resolver_plugin::test_support::request_builder::{
    DEFAULT_RESOURCE_TYPE, DEFAULT_SUBJECT_TYPE,
};
use authz_resolver_sdk::AuthZResolverError;
use rbac_sdk::error::RbacServiceError;
use rbac_sdk::models::{EffectivePermission, PermissionRule, PermissionScopeType, Scope};
use resource_group_sdk::models::{GroupHierarchyWithDepth, ResourceGroupWithDepth};
use secrecy::SecretString;
use tenant_resolver_sdk::models::{TenantId, TenantInfo, TenantRef, TenantStatus};
use toolkit::Gear;
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;

use common::{
    InMemoryRbacServiceClient, InMemoryResourceGroupClient, InMemoryTenantResolverClient,
    RecordingTypesRegistry,
};

const SCOPE_MISMATCH_V1: &str = "gts.cf.core.errors.err.v1~cf.authz.errors.scope_mismatch.v1";
const INSUFFICIENT_PERMISSIONS_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.insufficient_permissions.v1";
const UNKNOWN_RESOURCE_TYPE_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.unknown_resource_type.v1";
const CONSTRAINTS_UNAVAILABLE_V1: &str =
    "gts.cf.core.errors.err.v1~cf.authz.errors.constraints_unavailable.v1";

// ----- Tracing capture utility -----------------------------------------

#[derive(Debug, Clone)]
struct CapturedEvent {
    fields: HashMap<String, String>,
}

#[derive(Default)]
struct Captured {
    events: Mutex<Vec<CapturedEvent>>,
}

struct AuditLayer {
    captured: Arc<Captured>,
}

impl<S> Layer<S> for AuditLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "cf-authz.audit" {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.captured.events.lock().unwrap().push(CapturedEvent {
            fields: visitor.fields,
        });
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
}

/// Set up a per-test capture. Returns the shared captured-events handle plus
/// the dispatcher guard (drop it to unregister the subscriber).
fn install_capture() -> (Arc<Captured>, tracing::dispatcher::DefaultGuard) {
    let captured = Arc::new(Captured::default());
    let layer = AuditLayer {
        captured: Arc::clone(&captured),
    };
    let subscriber = tracing_subscriber::registry().with(layer);
    let guard = subscriber.set_default();
    (captured, guard)
}

// ----- Common helpers --------------------------------------------------

fn default_rbac_allow() -> Arc<InMemoryRbacServiceClient> {
    Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![],
        PermissionScopeType::Global,
    ))
}

fn root_tenant(id: u128) -> TenantInfo {
    TenantInfo {
        id: TenantId(Uuid::from_u128(id)),
        name: format!("t-{id:x}"),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}

fn scoped_grant(scope: Scope) -> EffectivePermission {
    EffectivePermission::new(
        PermissionRule::new("read", DEFAULT_RESOURCE_TYPE),
        Uuid::new_v4(),
        Uuid::new_v4(),
        "Scoped Reader",
        scope,
        false,
    )
}

/// Build valid assignment provenance whose tenant and group legs both resolve
/// to empty runtime sets.
fn valid_combined_allow_with_empty_materialization() -> (
    Arc<InMemoryRbacServiceClient>,
    Arc<InMemoryTenantResolverClient>,
    Arc<InMemoryResourceGroupClient>,
) {
    let tenant_id = Uuid::from_u128(0x7E01);
    let group_id = Uuid::from_u128(0x7E02);
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed(
        vec![
            scoped_grant(Scope::tenant(tenant_id)),
            scoped_grant(Scope::resource_group(tenant_id, group_id)),
        ],
        PermissionScopeType::Combined {
            scopes: vec![
                PermissionScopeType::TenantSubtree {
                    root_tenant_id: tenant_id,
                },
                PermissionScopeType::GroupSubtree {
                    root_group_ids: vec![group_id],
                },
            ],
        },
    ));
    let tenant = TenantInfo {
        id: TenantId(tenant_id),
        name: "suspended-empty-root".to_owned(),
        status: TenantStatus::Suspended,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        tenant.clone(),
    ]));
    tr.add_descendants(tenant.id, Vec::new());
    let rg = Arc::new(InMemoryResourceGroupClient::with_group_descendants(
        group_id,
        vec![ResourceGroupWithDepth {
            id: group_id,
            code: "gts.cf.core.rg.type.v1~empty.v1~".to_owned(),
            name: "empty-group".to_owned(),
            hierarchy: GroupHierarchyWithDepth {
                parent_id: None,
                tenant_id,
                depth: 0,
            },
            metadata: None,
        }],
    ));

    (rbac, tr, rg)
}

async fn init_plugin(
    rbac: Arc<InMemoryRbacServiceClient>,
    tr: Arc<InMemoryTenantResolverClient>,
    rg: Arc<InMemoryResourceGroupClient>,
    audit_enabled: Option<bool>,
    gts_mode: Option<&str>,
) -> (
    Arc<dyn authz_resolver_sdk::AuthZResolverPluginClient>,
    Arc<RecordingTypesRegistry>,
) {
    let (ctx, hub, registry, _rbac, _tr, _rg) = common::build_ctx_with_config(
        rbac,
        tr,
        rg,
        common::CtxOverrides {
            audit_enabled,
            gts_validation_mode: gts_mode.map(str::to_owned),
            ..Default::default()
        },
    );
    AuthZResolverPluginGear
        .init(&ctx)
        .await
        .expect("init should succeed");
    (common::resolve_plugin(&hub), registry)
}

fn build_request_with_known_types() -> authz_resolver_sdk::EvaluationRequest {
    // Default require_constraints=true so we exercise the constraint path.
    EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .build()
}

// ----- Tests -----------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn audit_explicitly_disabled_emits_no_events() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x1000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(false), None).await;
    common::register_default_types(&registry);

    _ = plugin.evaluate(build_request_with_known_types()).await;
    assert_eq!(
        captured.events.lock().unwrap().len(),
        0,
        "audit.enabled=false must emit no events"
    );
}

/// The default is ON: a PDP that decides without leaving an audit trail is a
/// missing operational control, so an unconfigured deployment must still audit.
#[tokio::test(flavor = "current_thread")]
async fn audit_is_enabled_when_the_deployment_says_nothing() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x1100);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    // `None` = the `audit:` key is absent from the config entirely.
    let (plugin, registry) = init_plugin(rbac, tr, rg, None, None).await;
    common::register_default_types(&registry);

    _ = plugin.evaluate(build_request_with_known_types()).await;
    assert_eq!(
        captured.events.lock().unwrap().len(),
        1,
        "an unconfigured `audit:` block must still audit the decision"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audit_enabled_emits_one_event_per_allow() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x2000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let response = plugin
        .evaluate(build_request_with_known_types())
        .await
        .expect("allow");
    assert!(response.decision);

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one audit event per evaluate()");
    let fields = &events[0].fields;
    assert_eq!(fields.get("decision").map(String::as_str), Some("true"));
    // constraints_count is now emitted as a typed u64 → captured as "1"
    // (no more "Some(1)" Debug rendering).
    assert_eq!(
        fields.get("constraints_count").map(String::as_str),
        Some("1"),
        "constraints_count should reflect the 1 emitted constraint: {fields:?}"
    );
    assert!(fields.contains_key("correlation_id"));
    assert!(fields.contains_key("latency_ms"));
    // An allow event must NOT carry a deny code (no stale deny_error_code).
    assert_eq!(
        fields.get("deny_error_code").map(String::as_str),
        Some(""),
        "allow event must have an empty deny_error_code"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audit_sanitizes_control_chars_in_action_no_log_injection() {
    // A caller-controlled action.name with an embedded newline (passes
    // validation — only `*`/`?` are rejected) must NOT reach the audit event
    // verbatim, or it could forge/split a log line under a text subscriber.
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x3000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned()])
        .with_action_name("read\n[forged] decision=true subject_id=victim")
        .build();
    _ = plugin.evaluate(request).await.expect("allow");

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let action = events[0]
        .fields
        .get("action")
        .expect("action field present");
    assert!(
        !action.contains('\n') && !action.contains('\r'),
        "audit action must not carry raw control chars (log injection): {action:?}"
    );
    assert!(
        action.contains('\u{FFFD}'),
        "control char should be replaced with U+FFFD: {action:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audit_enabled_emits_one_event_per_business_deny() {
    let (captured, _guard) = install_capture();

    // Scope mismatch — empty token_scopes + a default `read` action.
    let rbac = Arc::new(InMemoryRbacServiceClient::default()); // never reached
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec![]) // empty — scope deny
        .build();
    let response = plugin.evaluate(request).await.expect("scope deny");
    assert!(!response.decision);

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let fields = &events[0].fields;
    assert_eq!(fields.get("decision").map(String::as_str), Some("false"));
    assert_eq!(
        fields.get("deny_error_code").map(String::as_str),
        Some(SCOPE_MISMATCH_V1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn audit_does_not_fire_on_err_returns() {
    let (captured, _guard) = install_capture();

    // Tenant resolver in error mode → materialize_scope returns Err.
    let rbac = default_rbac_allow();
    let tr = Arc::new(InMemoryTenantResolverClient::with_error("simulated"));
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    match plugin.evaluate(build_request_with_known_types()).await {
        Err(AuthZResolverError::ServiceUnavailable(_)) => {}
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(
        captured.events.lock().unwrap().len(),
        0,
        "Err(_) returns must skip audit"
    );
}

/// A malformed request is a DECISION (`invalid_request.v1` deny), not an
/// infrastructure error, so it must leave an audit record like any other deny.
/// It used to propagate as `Internal` and skip audit entirely, which meant a
/// caller could be denied with nothing written down.
#[tokio::test(flavor = "current_thread")]
async fn audit_fires_on_a_client_fault_deny() {
    let (captured, _guard) = install_capture();

    let rbac = Arc::new(InMemoryRbacServiceClient::default());
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        // foundation validation rejects a present-but-unrecognized subject type
        // (absent is now valid → defaults to User)
        .with_subject_type(Some("bogus-type".to_owned()))
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("a client fault is Ok(decision=false), not Err");
    assert!(!response.decision);

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1, "a client-fault deny must be audited");
    let deny_code = events[0]
        .fields
        .get("deny_error_code")
        .map(String::as_str)
        .expect("audit record must carry deny_error_code");
    assert!(
        deny_code.ends_with("invalid_request.v1"),
        "the audit record must carry the client-fault deny code, got {deny_code}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn correlation_id_unique_per_evaluation() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x3000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    _ = plugin.evaluate(build_request_with_known_types()).await;
    _ = plugin.evaluate(build_request_with_known_types()).await;

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 2);
    let cid1 = events[0]
        .fields
        .get("correlation_id")
        .expect("correlation_id present");
    let cid2 = events[1]
        .fields
        .get("correlation_id")
        .expect("correlation_id present");
    assert_ne!(cid1, cid2, "correlation_ids must differ across calls");
}

#[tokio::test(flavor = "current_thread")]
async fn bearer_token_absent_from_emitted_event() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x4000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    // Build a request, then attach a known bearer token canary.
    let mut request = build_request_with_known_types();
    request.context.bearer_token = Some(SecretString::new("secret-canary-token-abc123".into()));

    _ = plugin.evaluate(request).await;

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let serialized: String = events[0]
        .fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    assert!(
        !serialized.contains("secret-canary-token-abc123"),
        "bearer token leaked into audit event: {serialized}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_predicate_values_absent_from_emitted_event() {
    let (captured, _guard) = install_capture();

    // The tenant uuid that ends up in the Eq predicate's value.
    let tenant_uuid = Uuid::from_u128(0x_CAFE_BABE_DEAD_BEEF_DEAD_BEEF_CAFE_BABE);
    let rbac = default_rbac_allow();
    let root = TenantInfo {
        id: TenantId(tenant_uuid),
        name: "canary".to_owned(),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    };
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    _ = plugin.evaluate(build_request_with_known_types()).await;

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let serialized: String = events[0]
        .fields
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(";");
    assert!(
        !serialized.contains(&tenant_uuid.to_string()),
        "raw predicate value leaked into audit event: {serialized}"
    );
    // The hash IS allowed.
    assert!(
        events[0].fields.contains_key("constraints_hash"),
        "constraints_hash should be present"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn require_constraints_false_allow_with_empty_constraints() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x5000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_require_constraints(false)
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("require_constraints=false allow");
    assert!(response.decision);
    assert!(response.context.constraints.is_empty());

    // Audit event reflects the empty-constraints allow.
    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let fields = &events[0].fields;
    assert_eq!(fields.get("decision").map(String::as_str), Some("true"));
    // Typed emit: constraints_count = 0; absent hash renders as "".
    assert_eq!(
        fields.get("constraints_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(fields.get("constraints_hash").map(String::as_str), Some(""));
}

#[tokio::test(flavor = "current_thread")]
async fn strict_mode_unknown_subject_returns_business_deny_via_audit() {
    let (captured, _guard) = install_capture();

    // Strict mode + no types primed → Strict + Unknown business deny.
    let rbac = default_rbac_allow();
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, _registry) = init_plugin(rbac, tr, rg, Some(true), Some("strict")).await;
    // Do NOT register default types — Strict will deny.

    let response = plugin
        .evaluate(build_request_with_known_types())
        .await
        .expect("Strict+Unknown is business deny, not Err");
    assert!(!response.decision);
    assert_eq!(
        response
            .context
            .deny_reason
            .as_ref()
            .map_or("", |d| d.error_code.as_str()),
        UNKNOWN_RESOURCE_TYPE_V1
    );

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].fields.get("deny_error_code").map(String::as_str),
        Some(UNKNOWN_RESOURCE_TYPE_V1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn pipeline_short_circuit_scope_before_rbac() {
    let (captured, _guard) = install_capture();

    let rbac = Arc::new(InMemoryRbacServiceClient::default()); // would error if called
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(Arc::clone(&rbac), tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec![]) // empty → scope deny
        .build();
    let response = plugin.evaluate(request).await.expect("scope deny");
    assert!(!response.decision);
    assert_eq!(rbac.call_count(), 0, "RBAC must never be invoked");
    assert_eq!(captured.events.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn valid_empty_materialization_returns_constraints_unavailable_and_is_audited() {
    let (captured, _guard) = install_capture();
    let (rbac, tr, rg) = valid_combined_allow_with_empty_materialization();
    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let request = EvaluationRequestBuilder::default()
        .with_token_scopes(vec!["*".to_owned()])
        .with_supported_properties(vec!["owner_tenant_id".to_owned(), "id".to_owned()])
        .with_require_constraints(true)
        .build();
    let response = plugin
        .evaluate(request)
        .await
        .expect("empty materialization is a business deny");
    assert!(!response.decision);
    assert_eq!(
        response
            .context
            .deny_reason
            .as_ref()
            .map_or("", |reason| reason.error_code.as_str()),
        CONSTRAINTS_UNAVAILABLE_V1
    );

    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].fields.get("deny_error_code").map(String::as_str),
        Some(CONSTRAINTS_UNAVAILABLE_V1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_combined_provenance_error_is_audited_before_hierarchy() {
    let (captured, _guard) = install_capture();

    // Empty assignment provenance is an RBAC contract violation, not a
    // constraint-generation outcome. It must stop before hierarchy I/O and
    // emit one bounded fail-closed audit decision before returning Internal.
    let rbac = Arc::new(InMemoryRbacServiceClient::with_allowed_mismatched(
        vec![],
        PermissionScopeType::Combined { scopes: vec![] },
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr.clone(), rg, Some(true), None).await;
    common::register_default_types(&registry);

    let error = plugin
        .evaluate(build_request_with_known_types())
        .await
        .expect_err("empty Combined must fail provenance validation");
    match error {
        AuthZResolverError::Internal(message) => {
            assert_eq!(
                message,
                "rbac allow has invalid assignment-scope provenance"
            );
        }
        other => panic!("expected provenance Internal error, got {other:?}"),
    }
    assert_eq!(
        tr.call_count(),
        0,
        "provenance failure must precede hierarchy I/O"
    );
    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1, "provenance rejection must be audited");
    assert_eq!(
        events[0].fields.get("decision").map(String::as_str),
        Some("false")
    );
    assert_eq!(
        events[0].fields.get("deny_error_code").map(String::as_str),
        Some(CONSTRAINTS_UNAVAILABLE_V1)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn subject_tenant_id_parsed_from_subject_properties() {
    let (captured, _guard) = install_capture();

    let rbac = default_rbac_allow();
    let root = root_tenant(0x6000);
    let tr = Arc::new(InMemoryTenantResolverClient::with_tenants(vec![
        root.clone(),
    ]));
    tr.add_descendants(root.id, vec![]);
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    let tenant_uuid = Uuid::from_u128(0xFEED_FACE);
    let mut request = build_request_with_known_types();
    request.subject.properties.insert(
        "tenant_id".to_owned(),
        serde_json::json!(tenant_uuid.to_string()),
    );

    _ = plugin.evaluate(request).await;
    let events = captured.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    // Emitted as a typed Display string — assert exact equality, not a
    // Debug-rendered substring.
    assert_eq!(
        events[0]
            .fields
            .get("subject_tenant_id")
            .map(String::as_str),
        Some(tenant_uuid.to_string().as_str()),
        "subject_tenant_id should carry exactly the parsed UUID"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rbac_unavailable_returns_err_without_audit() {
    // System error (RBAC unreachable) → Err(ServiceUnavailable), no audit.
    let (captured, _guard) = install_capture();

    let rbac = Arc::new(InMemoryRbacServiceClient::with_error(
        RbacServiceError::internal("simulated"),
    ));
    let tr = Arc::new(InMemoryTenantResolverClient::default());
    let rg = Arc::new(InMemoryResourceGroupClient::default());

    let (plugin, registry) = init_plugin(rbac, tr, rg, Some(true), None).await;
    common::register_default_types(&registry);

    match plugin
        .evaluate(
            EvaluationRequestBuilder::default()
                .with_token_scopes(vec!["*".to_owned()])
                .build(),
        )
        .await
    {
        Err(AuthZResolverError::ServiceUnavailable(_)) => {}
        other => panic!("expected ServiceUnavailable, got {other:?}"),
    }
    assert_eq!(captured.events.lock().unwrap().len(), 0);
}

// Silence unused-import warnings for symbols only used by specific tests.
#[allow(dead_code)]
const _SUBJECT_REACHABLE: &str = DEFAULT_SUBJECT_TYPE;
#[allow(dead_code)]
const _RESOURCE_REACHABLE: &str = DEFAULT_RESOURCE_TYPE;
#[allow(dead_code)]
const _INSUFFICIENT_REACHABLE: &str = INSUFFICIENT_PERMISSIONS_V1;
#[allow(dead_code)]
fn _tenant_ref_reachable() -> TenantRef {
    TenantRef {
        id: TenantId(Uuid::nil()),
        status: TenantStatus::Active,
        tenant_type: None,
        parent_id: None,
        self_managed: false,
    }
}
