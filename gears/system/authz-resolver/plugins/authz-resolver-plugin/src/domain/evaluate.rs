//! The `AuthZResolverPluginClient` implementation.
//!
//! `evaluate()` is the only trait method on the SDK trait. The 10-step pipeline:
//! validate → `gts_type_validator` → scope enforcer → policy evaluator → scope
//! provenance validation → `materialize_scope` → non-empty materialization
//! enforcement → `require_constraints` branch → constraint generator → audit
//! emission. Every `Ok(response)` return is audited. Client-fault failures (a
//! malformed `AuthZEN` request) become audited `invalid_request.v1` denies rather
//! than plugin errors. Remaining infrastructure `Err(_)` paths skip audit except
//! RBAC provenance rejection, which emits a bounded fail-closed deny record
//! before returning its typed internal error.

// This orchestrator legitimately holds an `AuthZMetrics` handle from the infra
// layer (a runtime adapter, not business logic). Same sanctioned exception the
// keycloak-authn-plugin domain uses for its infra adapter handles.
#![allow(unknown_lints, de0301_no_infra_in_domain)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use authz_resolver_sdk::models::{BarrierMode, Capability};
use authz_resolver_sdk::plugin_api::AuthZResolverPluginClient;
use authz_resolver_sdk::{AuthZResolverError, EvaluationRequest, EvaluationResponse};

use crate::domain::error::PluginError;
use crate::infra::hierarchy_upstream::SdkHierarchyUpstream;
use rbac_sdk::RbacServiceClientV1;
use resource_group_sdk::api::ResourceGroupReadHierarchy;
use tenant_resolver_sdk::api::TenantResolverClient;
use tracing::warn;
use types_registry_sdk::TypesRegistryClient;
use uuid::Uuid;

use crate::config::AuthZResolverPluginConfig;
use crate::domain::audit_emitter::{AuditEmitter, AuditRecord};
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::constraint_generator::{ConstraintOutcome, generate_constraints};
use crate::domain::deny::build_allow_response;
use crate::domain::deny::build_deny_response;
use crate::domain::deny::error_codes::{
    CONSTRAINTS_UNAVAILABLE_V1, INSUFFICIENT_PERMISSIONS_V1, INVALID_REQUEST_V1,
    UNSUPPORTED_PROPERTY_V1,
};
use crate::domain::gts_type_validator::{GtsTypeValidator, TypeValidationOutcome};
use crate::domain::hierarchy_cache::HierarchyCache;
use crate::domain::hierarchy_client::{HierarchyClient, Materialization};
use crate::domain::metrics_port::{ErrorType, NarrowingOp, RbacOp, ScopeTypeLabel};
use crate::domain::policy_evaluator::{PolicyEvaluator, PolicyOutcome};
use crate::domain::scope_enforcer::ScopeEnforcer;
use crate::domain::validation::validate;
use crate::infra::metrics::AuthZMetrics;
use rbac_sdk::models::PermissionScopeType;
use toolkit_macros::domain_model;

/// The `AuthZ` Resolver Plugin's evaluation orchestrator.
///
/// Holds the four downstream `ClientHub` dependencies and the sub-components
/// each pipeline step delegates to. See `docs/DESIGN.md` section 3.1 for the
/// step order and why it is the order it is.
#[domain_model]
pub(crate) struct AuthZResolverPlugin {
    config: Arc<AuthZResolverPluginConfig>,
    rbac: Arc<dyn RbacServiceClientV1>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    resource_group: Arc<dyn ResourceGroupReadHierarchy>,
    types_registry: Arc<dyn TypesRegistryClient>,

    trusted_system_actors: crate::domain::subject_type::TrustedSystemActors,
    scope_enforcer: ScopeEnforcer,
    policy_evaluator: PolicyEvaluator,
    hierarchy_client: HierarchyClient,
    gts_type_validator: GtsTypeValidator,
    audit_emitter: AuditEmitter,
    metrics: Arc<AuthZMetrics>,
}

impl AuthZResolverPlugin {
    /// Production constructor: binds metrics to the process-global meter
    /// provider (a no-op until the host installs an exporter).
    pub(crate) fn new(
        config: Arc<AuthZResolverPluginConfig>,
        rbac: Arc<dyn RbacServiceClientV1>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        resource_group: Arc<dyn ResourceGroupReadHierarchy>,
        types_registry: Arc<dyn TypesRegistryClient>,
    ) -> Self {
        Self::with_metrics(
            config,
            rbac,
            tenant_resolver,
            resource_group,
            types_registry,
            Arc::new(AuthZMetrics::from_global()),
        )
    }

    /// Constructor with an injected metrics handle. Tests pass a handle bound
    /// to a `MetricsHarness` to assert emitted metrics; `new` uses the global
    /// meter for production.
    pub(crate) fn with_metrics(
        config: Arc<AuthZResolverPluginConfig>,
        rbac: Arc<dyn RbacServiceClientV1>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        resource_group: Arc<dyn ResourceGroupReadHierarchy>,
        types_registry: Arc<dyn TypesRegistryClient>,
        metrics: Arc<AuthZMetrics>,
    ) -> Self {
        // Clone the config / rbac Arc handles once before the moves so the
        // ScopeEnforcer and PolicyEvaluator can keep their own references.
        let scope_enforcer = ScopeEnforcer::new(Arc::clone(&config));
        // Compiled once from config: the PDP's trusted-actor bypass surface.
        let trusted_system_actors = crate::domain::subject_type::TrustedSystemActors::from_config(
            &config.trusted_system_actors,
        );
        let policy_evaluator =
            PolicyEvaluator::new(Arc::clone(&rbac), trusted_system_actors.clone());
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let cache = Arc::new(HierarchyCache::new(
            &config.cache,
            Arc::clone(&clock),
            Arc::clone(&metrics),
        ));
        let hierarchy_upstream = Arc::new(SdkHierarchyUpstream::new(
            Arc::clone(&tenant_resolver),
            Arc::clone(&resource_group),
            Arc::clone(&metrics),
        ));
        let hierarchy_client = HierarchyClient::new(hierarchy_upstream, cache);
        let gts_type_validator = GtsTypeValidator::new(
            config.gts_validation.mode,
            Arc::clone(&types_registry),
            Duration::from_secs(config.cache.ttl_seconds),
            clock,
        );
        let audit_emitter = AuditEmitter::new(config.audit.enabled);
        Self {
            config,
            rbac,
            tenant_resolver,
            resource_group,
            types_registry,
            trusted_system_actors,
            scope_enforcer,
            policy_evaluator,
            hierarchy_client,
            gts_type_validator,
            audit_emitter,
            metrics,
        }
    }

    // Accessors for the downstream clients, so callers reach them without
    // touching the struct definition.
    #[allow(dead_code)]
    pub(crate) fn config(&self) -> &AuthZResolverPluginConfig {
        &self.config
    }

    #[allow(dead_code)]
    pub(crate) fn rbac(&self) -> &Arc<dyn RbacServiceClientV1> {
        &self.rbac
    }

    #[allow(dead_code)]
    pub(crate) fn tenant_resolver(&self) -> &Arc<dyn TenantResolverClient> {
        &self.tenant_resolver
    }

    #[allow(dead_code)]
    pub(crate) fn resource_group(&self) -> &Arc<dyn ResourceGroupReadHierarchy> {
        &self.resource_group
    }

    // Accessors for the pipeline components.
    #[allow(dead_code)]
    pub(crate) fn scope_enforcer(&self) -> &ScopeEnforcer {
        &self.scope_enforcer
    }

    #[allow(dead_code)]
    pub(crate) fn policy_evaluator(&self) -> &PolicyEvaluator {
        &self.policy_evaluator
    }

    #[allow(dead_code)]
    pub(crate) fn hierarchy_client(&self) -> &HierarchyClient {
        &self.hierarchy_client
    }

    #[allow(dead_code)]
    pub(crate) fn gts_type_validator(&self) -> &GtsTypeValidator {
        &self.gts_type_validator
    }

    #[allow(dead_code)]
    pub(crate) fn types_registry(&self) -> &Arc<dyn TypesRegistryClient> {
        &self.types_registry
    }

    #[allow(dead_code)]
    pub(crate) fn audit_emitter(&self) -> &AuditEmitter {
        &self.audit_emitter
    }

    /// Build an audit record from the outgoing response, emit, then return
    /// `Ok(response)`. Consolidates the "build → emit → return" three-step
    /// at every `Ok(_)` exit of `evaluate()`. Returns `Result`
    /// (always `Ok`) so call sites can `return self.audit_and_return(...)`
    /// directly in `evaluate()`'s `Result`-typed body.
    #[allow(clippy::unnecessary_wraps)]
    fn audit_and_return(
        &self,
        start: Instant,
        correlation_id: Uuid,
        request: &EvaluationRequest,
        response: EvaluationResponse,
    ) -> Result<EvaluationResponse, PluginError> {
        let record =
            AuditRecord::from_response(correlation_id, request, start.elapsed(), &response);
        self.audit_emitter.emit(&record);
        Ok(response)
    }

    /// Emit a fail-closed audit decision and then return the supplied system
    /// error. RBAC provenance rejection uses this narrow exception because the
    /// malformed allow reached the decision pipeline and must remain auditable,
    /// while the SDK still surfaces producer contract drift as an internal
    /// error. Other infrastructure errors continue to skip decision auditing.
    fn audit_error_and_return(
        &self,
        start: Instant,
        correlation_id: Uuid,
        request: &EvaluationRequest,
        audit_response: &EvaluationResponse,
        error: PluginError,
    ) -> Result<EvaluationResponse, PluginError> {
        let record =
            AuditRecord::from_response(correlation_id, request, start.elapsed(), audit_response);
        self.audit_emitter.emit(&record);
        Err(error)
    }

    /// Project a pipeline failure onto the response boundary.
    ///
    /// A client fault — a malformed `AuthZEN` request — is a business deny, not a
    /// plugin failure. `AuthZResolverError` has no `InvalidRequest` variant, so
    /// propagating one reaches the PEP as a 500-class `Internal`: PEPs retry it
    /// and on-call is paged for a caller's typo that no retry can fix. Returning
    /// an audited `invalid_request.v1` deny keeps the decision on the same path
    /// as every other business denial.
    ///
    /// The client-fault/system-fault split is read off
    /// [`PluginError::labels`] rather than re-listed here: that match is
    /// exhaustive with no wildcard arm, so a new failure mode cannot reach this
    /// boundary unclassified.
    fn deny_client_fault_or_propagate(
        &self,
        start: Instant,
        correlation_id: Uuid,
        request: &EvaluationRequest,
        error: PluginError,
    ) -> Result<EvaluationResponse, PluginError> {
        if error.labels().0 != ErrorType::InvalidRequest {
            return Err(error);
        }
        // The detail echoes the caller their own malformed field; the audit
        // emitter sanitizes control characters before the record is logged.
        let response = build_deny_response(INVALID_REQUEST_V1, Some(error.to_string()));
        self.audit_and_return(start, correlation_id, request, response)
    }
}

#[async_trait]
impl AuthZResolverPluginClient for AuthZResolverPlugin {
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        // Thin wrapper: time the whole evaluation and record outcome metrics
        // on every return path (Ok allow/deny and Err), then return the inner
        // result unchanged (§3.13 authz_evaluation_{duration,deny,error,fail_closed}).
        let start = Instant::now();
        let result = self.evaluate_inner(start, request).await;
        // Classify BEFORE projecting onto the SDK error: `PluginError` carries
        // the metric labels on its variants, and the projection is lossy.
        self.metrics.record_outcome(start.elapsed(), &result);
        result.map_err(Into::into)
    }
}

impl AuthZResolverPlugin {
    /// The 10-step evaluation pipeline. `start` is supplied by the trait
    /// wrapper so audit latency and metric latency share one clock reading.
    // One guard per pipeline step, in the order the steps run: the branch count
    // IS the pipeline, and splitting it across helpers would hide that order
    // from the only place a reader can see it.
    #[allow(clippy::cognitive_complexity)]
    async fn evaluate_inner(
        &self,
        start: Instant,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, PluginError> {
        // Step 0 — correlation_id (timing captured by the wrapper).
        let correlation_id = Uuid::new_v4();

        // Step 1 — request-shape validation. A malformed request is the
        // caller's fault, so it becomes an audited `invalid_request.v1` deny
        // rather than a plugin error the PEP would retry.
        if let Err(error) = validate(&request, &self.trusted_system_actors) {
            return self.deny_client_fault_or_propagate(start, correlation_id, &request, error);
        }

        // Step 2 — GTS type validation. Strict + Unknown surfaces as a
        // business deny (Ok(Deny(_))); registry outage as `Err`.
        match self
            .gts_type_validator
            .validate_request(&request, &self.trusted_system_actors)
            .await?
        {
            TypeValidationOutcome::Allow => {}
            TypeValidationOutcome::Deny(response) => {
                return self.audit_and_return(start, correlation_id, &request, response);
            }
        }

        // Step 3 — scope enforcement. Skipped for the unforgeable
        // in-process trusted system actor: it is constructed
        // in-process with no token at all, so `token_scopes` is empty by
        // construction and the fail-closed empty-scopes deny would block
        // every retention/cascade sweep (observed: the hard-delete reaper
        // deferring its whole backlog forever). Scope enforcement guards
        // token-bearing callers; the system actor is admitted by the same
        // sentinel-id gate that powers the Step 4 short-circuit, so a
        // forged `subject_type` alone cannot ride this bypass.
        let trusted_system_actor = self
            .trusted_system_actors
            .matches(request.subject.id, request.subject.subject_type.as_deref());
        if !trusted_system_actor
            && let Err(deny) = self
                .scope_enforcer
                .check_scopes(&request.context.token_scopes, &request.action.name)
        {
            return self.audit_and_return(start, correlation_id, &request, deny);
        }

        // Observability for the admitted request: capability set,
        // token-scope narrowing, and cross-barrier override (§3.13 counters).
        self.emit_request_metrics(&request);

        // Step 4 — policy evaluation (RBAC). Time the in-process call (§3.13
        // authz_rbac_query_duration_milliseconds), including the error path.
        let rbac_start = Instant::now();
        let policy_outcome = self.policy_evaluator.evaluate_permissions(&request).await;
        self.metrics
            .record_rbac_query(RbacOp::EvaluatePermission, rbac_start.elapsed());
        let granted = match policy_outcome {
            // `map_subject_type` and the subject-tenant read raise client faults
            // here too, so this boundary needs the same projection as Step 1.
            Err(error) => {
                return self.deny_client_fault_or_propagate(start, correlation_id, &request, error);
            }
            Ok(PolicyOutcome::Denied(response)) => {
                return self.audit_and_return(start, correlation_id, &request, response);
            }
            Ok(PolicyOutcome::Allowed(granted)) => granted,
        };

        // Step 5 — assignment-scope provenance. A normal RBAC allow must prove
        // its aggregate scope from the role assignments that contributed it.
        // Validate before recording the allow shape or performing hierarchy
        // I/O so producer drift, stale data, or partial payload corruption
        // cannot widen into platform-root materialization. This is consistency
        // validation, not an independent anti-forgery boundary: a future remote
        // transport must authenticate and integrity-protect the whole payload.
        // The sole exception is the already-authenticated in-process system
        // actor, whose explicit empty-grant allow was constructed above and is
        // not backed by persisted RBAC assignments.
        if !trusted_system_actor
            && scope_can_materialize_allow(&granted.scope_type)
            && let Err(error) = granted.validate_scope_provenance()
        {
            self.metrics.inc_scope_provenance_rejection();
            warn!(
                %error,
                "RBAC allow has inconsistent assignment-scope provenance; failing closed"
            );
            let audit_response = build_deny_response(
                CONSTRAINTS_UNAVAILABLE_V1,
                Some("RBAC allow scope does not match contributing role assignments".to_owned()),
            );
            return self.audit_error_and_return(
                start,
                correlation_id,
                &request,
                &audit_response,
                PluginError::RbacScopeProvenanceInvalid,
            );
        }

        // RBAC allowed — record the scope type distribution (§3.13
        // authz_evaluation_by_scope_type_total).
        let scope_label = scope_type_label(&granted.scope_type);
        self.metrics.record_scope_type(scope_label);

        // Step 6 — materialize the granted scope. Reserved-variant denies
        // surface as `Materialization::Denied` (handled by the constraint
        // generator below).
        let materialized = self
            .hierarchy_client
            .materialize_scope(&granted.scope_type, &request)
            .await?;

        // Step 7 — an allowed scope must resolve to at least one accessible
        // tenant/resource ID before the decision-only branch can discard
        // constraints. In particular, Combined(tenant=[], resource=[]) must
        // never become an unconstrained allow when `require_constraints=false`.
        if let Some((error_code, details)) = empty_materialization_deny(&materialized) {
            warn!(
                error_code,
                "RBAC allow materialized to no accessible IDs; failing closed"
            );
            return self.audit_and_return(
                start,
                correlation_id,
                &request,
                build_deny_response(error_code, Some(details.to_owned())),
            );
        }

        // Step 8 — require_constraints branching. When the PEP
        // declares `require_constraints=false` AND the materialization is
        // not a reserved-variant deny, skip constraint generation entirely
        // and return an empty-constraints allow.
        let materialization_is_denied = matches!(materialized, Materialization::Denied { .. });
        if !request.context.require_constraints && !materialization_is_denied {
            return self.audit_and_return(
                start,
                correlation_id,
                &request,
                build_allow_response(vec![]),
            );
        }

        // Step 9 — constraint generation. Denied materializations and
        // require_constraints=true paths both flow through here. Time the
        // compilation (§3.13 authz_constraint_compilation_duration_milliseconds).
        let cg_start = Instant::now();
        let cg_outcome = generate_constraints(&materialized, &request, &self.config);
        self.metrics
            .record_constraint_compilation(scope_label, cg_start.elapsed());
        // Unsupported-property denials signal a PEP config issue (§3.13
        // authz_unsupported_property_total).
        if let ConstraintOutcome::Deny(resp) = &cg_outcome
            && resp
                .context
                .deny_reason
                .as_ref()
                .map(|d| d.error_code.as_str())
                == Some(UNSUPPORTED_PROPERTY_V1)
        {
            self.metrics.inc_unsupported_property();
        }
        let response = match cg_outcome {
            ConstraintOutcome::Allow(constraints) if constraints.is_empty() => {
                // require_constraints=true (guaranteed by the branch above)
                // but constraint generator produced none → defensible deny.
                // This signals an upstream RBAC contract drift (e.g. Combined
                // with both sides empty); log so operators can spot it without
                // having to scrape `fail_closed_total{reason="constraints_unavailable"}`.
                warn!(
                    "constraints_unavailable: constraint generator produced no constraints \
                     under require_constraints=true (likely upstream RBAC contract drift)"
                );
                build_deny_response(
                    CONSTRAINTS_UNAVAILABLE_V1,
                    Some(
                        "require_constraints=true but constraint generator produced none"
                            .to_owned(),
                    ),
                )
            }
            ConstraintOutcome::Allow(constraints) => build_allow_response(constraints),
            ConstraintOutcome::Deny(response) => response,
        };

        // Step 10 — audit + return.
        self.audit_and_return(start, correlation_id, &request, response)
    }

    /// Emit per-request observability counters for an admitted request:
    /// capability set, token-scope narrowing, and cross-barrier override.
    fn emit_request_metrics(&self, request: &EvaluationRequest) {
        // PEP-declared capability set (mapped to stable strings, sorted).
        let mut caps: Vec<&'static str> = request
            .context
            .capabilities
            .iter()
            .map(|c| match c {
                Capability::TenantHierarchy => "tenant_hierarchy",
                Capability::GroupMembership => "group_membership",
                Capability::GroupHierarchy => "group_hierarchy",
            })
            .collect();
        caps.sort_unstable();
        self.metrics.inc_capability_negotiation(&caps.join(","));

        // Token-scope narrowing: a non-empty, non-wildcard token caps a
        // third-party app below the user's full permissions.
        let wildcard = self.config.scope_enforcement.wildcard_scope.as_str();
        let scopes = &request.context.token_scopes;
        if !scopes.is_empty() && !scopes.iter().any(|s| s == wildcard) {
            // Bounded label only (raw action.name would be unbounded cardinality).
            self.metrics
                .inc_token_scope_narrowing(NarrowingOp::from_action(&request.action.name));
        }

        // Cross-barrier override (billing/admin operations).
        if let Some(tc) = request.context.tenant_context.as_ref()
            && tc.barrier_mode == BarrierMode::Ignore
        {
            self.metrics.inc_barrier_mode_override();
        }
    }
}

/// Whether an RBAC scope can reach an allow materialization and therefore
/// requires assignment-provenance consistency validation at this boundary.
///
/// Reserved variants and a `Combined` containing a reserved leg already fail
/// closed in hierarchy materialization. Keeping them on that established
/// business-deny path preserves the resolver's public deny taxonomy. An empty
/// `Combined` is different: materialization produces an empty, non-denied value,
/// which a decision-only PEP would otherwise accept. It must therefore enter
/// provenance validation and be rejected as an impossible normal RBAC allow.
/// Unknown future variants are checked and rejected because no v1 assignment
/// can canonically derive them.
fn scope_can_materialize_allow(scope: &PermissionScopeType) -> bool {
    match scope {
        PermissionScopeType::Combined { scopes } => {
            scopes.is_empty() || scopes.iter().all(scope_can_materialize_allow)
        }
        PermissionScopeType::TenantDirect { .. } | PermissionScopeType::ExplicitGroups { .. } => {
            false
        }
        // Global, TenantSubtree, GroupSubtree, and unknown future variants
        // must all prove their assignment provenance before materialization.
        _ => true,
    }
}

/// Return the fail-closed deny for a successful hierarchy materialization that
/// resolved no tenant or resource IDs and therefore represents authority over
/// an empty set.
///
/// Single tenant/group scopes preserve their established
/// `insufficient_permissions.v1` outcome. An empty `Combined` uses
/// `constraints_unavailable.v1` because its two individually valid authority
/// paths jointly failed to produce any enforceable target. Push-down
/// materialization is non-empty by construction because it carries a concrete
/// root; reserved `Denied` values are handled by constraint generation.
fn empty_materialization_deny(
    materialization: &Materialization,
) -> Option<(&'static str, &'static str)> {
    match materialization {
        Materialization::TenantSubtree { tenant_ids } if tenant_ids.is_empty() => Some((
            INSUFFICIENT_PERMISSIONS_V1,
            "tenant subtree resolved to no accessible tenants",
        )),
        Materialization::GroupSubtree { resource_ids, .. } if resource_ids.is_empty() => Some((
            INSUFFICIENT_PERMISSIONS_V1,
            "group scope resolved to no member resources",
        )),
        Materialization::Combined {
            tenant_ids,
            resource_ids,
            ..
        } if tenant_ids.is_empty() && resource_ids.is_empty() => Some((
            CONSTRAINTS_UNAVAILABLE_V1,
            "combined RBAC allow scope materialized to no accessible IDs",
        )),
        Materialization::TenantDirect { .. }
        | Materialization::TenantSubtree { .. }
        | Materialization::TenantSubtreePushdown { .. }
        | Materialization::GroupSubtree { .. }
        | Materialization::Combined { .. }
        | Materialization::Denied { .. } => None,
    }
}

/// Map an RBAC `PermissionScopeType` to a typed metric label for
/// `authz.evaluation_by_scope_type{scope_type=...}`. Returns the bounded
/// `ScopeTypeLabel` enum so the metric cardinality is fixed at compile time.
fn scope_type_label(scope: &PermissionScopeType) -> ScopeTypeLabel {
    match scope {
        PermissionScopeType::Global => ScopeTypeLabel::Global,
        PermissionScopeType::TenantSubtree { .. } => ScopeTypeLabel::TenantSubtree,
        PermissionScopeType::TenantDirect { .. } => ScopeTypeLabel::TenantDirect,
        PermissionScopeType::GroupSubtree { .. } => ScopeTypeLabel::GroupSubtree,
        PermissionScopeType::ExplicitGroups { .. } => ScopeTypeLabel::ExplicitGroups,
        PermissionScopeType::Combined { .. } => ScopeTypeLabel::Combined,
        // `PermissionScopeType` is #[non_exhaustive]; a future variant should
        // surface as a distinct label rather than fail the build.
        _ => ScopeTypeLabel::Other,
    }
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod tests;
