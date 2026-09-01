//! Real RBAC permission-evaluation step — replaces `PolicyEvaluatorStub`.
//!
//! Sits between scope enforcement and (still-stubbed) constraint generation
//! in the `evaluate()` pipeline. Two stateless helpers (`map_subject_type`,
//! `evaluation_context_to_scope`) translate from the `AuthZ` SDK shape to the
//! RBAC SDK shape; `evaluate_permissions` orchestrates the RBAC call and
//! maps the result back.
//!
//! Outcomes:
//! - RBAC `Ok(Allowed(...))` → `Ok(PolicyOutcome::Allowed(granted))` —
//!   caller continues to the next pipeline step, which consumes
//!   `granted.scope_type` to materialize constraints.
//! - RBAC `Ok(Denied(...))` → `Ok(PolicyOutcome::Denied(response))` where
//!   `response` carries `INSUFFICIENT_PERMISSIONS_V1`.
//! - RBAC `Err(...)` → `Err(PluginError::RbacUnavailable)`.
//!
//! The plugin does NOT reinterpret RBAC's additive-union or `not_permissions`
//! semantics — those are the RBAC service's responsibility. The plugin is a
//! thin translator.
use std::sync::Arc;

use crate::domain::error::PluginError;
use authz_resolver_sdk::models::{EvaluationRequest, EvaluationRequestContext, EvaluationResponse};
use rbac_sdk::RbacServiceClientV1;
use rbac_sdk::models::{
    EvaluatePermissionRequest, PermissionGranted, PermissionResult, PermissionScopeType,
    PrincipalType, Scope,
};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::domain::deny::{build_deny_response, error_codes};
use crate::domain::subject_type::{TrustedSystemActors, classify_subject_type};
use toolkit_macros::domain_model;

/// Outcome of one policy-evaluation call.
///
/// Three plugin behaviors collapse into two outcomes plus an `Err`:
/// - RBAC Allowed → `Allowed(granted)`, caller continues
/// - RBAC Denied → `Denied(response)`, caller returns the response directly
/// - RBAC Err → `evaluate_permissions` returns the outer `Err`
#[domain_model]
#[derive(Debug)]
pub(crate) enum PolicyOutcome {
    Allowed(PermissionGranted),
    Denied(EvaluationResponse),
}

#[domain_model]
pub(crate) struct PolicyEvaluator {
    rbac: Arc<dyn RbacServiceClientV1>,
    /// Configured in-process actors short-circuited to Allow; empty unless the
    /// deployment named some.
    trusted: TrustedSystemActors,
}

impl PolicyEvaluator {
    pub(crate) fn new(rbac: Arc<dyn RbacServiceClientV1>, trusted: TrustedSystemActors) -> Self {
        Self { rbac, trusted }
    }

    /// Map a `subject_type` string to an RBAC `PrincipalType` via the shared
    /// tolerant [`classify_subject_type`] (raw `IdP` claim values + GTS tags).
    /// Pure function — validation already rejects malformed values, but this
    /// reports the same errors with identical wording for defense in depth
    /// and to keep the function unit-testable in isolation.
    pub(crate) fn map_subject_type(
        subject_type: Option<&str>,
    ) -> Result<PrincipalType, PluginError> {
        match subject_type {
            // Absent `subject_type` defaults to `User`, mirroring RBAC's
            // `principal_type_from_security_context`. Deviates from DESIGN
            // §3.5's fail-closed stance to stay consistent with the RBAC
            // service, which some create/internal authz paths reach with no
            // tag (observed on the shared cluster).
            None => Ok(PrincipalType::User),
            Some(value) => {
                classify_subject_type(value).ok_or_else(|| PluginError::UnknownSubjectType {
                    value: value.to_owned(),
                })
            }
        }
    }

    // Error-taxonomy ladder over `RbacServiceError` variants.
    #[allow(clippy::cognitive_complexity)]
    pub(crate) async fn evaluate_permissions(
        &self,
        request: &EvaluationRequest,
    ) -> Result<PolicyOutcome, PluginError> {
        // Configured trusted system actors short-circuit to Allow. These are
        // in-process workers whose reads happen to be PEP-gated — a cleanup or
        // cascade job, typically — that hold no RBAC roles and so would
        // otherwise be denied outright. The gate is the unforgeable subject id
        // (minted in-process, never issued to a token holder), not the type
        // string alone, so a forged `subject_type` cannot impersonate one.
        //
        // This is a bypass, which is why nothing is trusted unless a deployment
        // names it. The structural fix is for such workers to reach their data
        // through an explicitly unscoped read contract instead of the PEP, at
        // which point they never arrive here at all; this branch exists because
        // that contract is not universally available.
        if self
            .trusted
            .matches(request.subject.id, request.subject.subject_type.as_deref())
        {
            // A nil home tenant is the platform-scoped system-actor shape:
            // the actor is constructed without a home tenant and the PEP
            // forwards the nil sentinel verbatim in
            // `properties["tenant_id"]`. It must grant Global, because a
            // subtree rooted at the nil sentinel materializes to zero
            // tenants and fails closed. The same hazard bit a hard-delete
            // cascade that ran rooted at the very tenant being deleted:
            // deleted roots are clamped out of the allow-set, so the
            // subtree came back empty and the worker denied itself.
            let scope_type = subject_home_tenant(request)?
                .filter(|root| !root.is_nil())
                .map_or(PermissionScopeType::Global, |root_tenant_id| {
                    PermissionScopeType::TenantSubtree { root_tenant_id }
                });
            return Ok(PolicyOutcome::Allowed(PermissionGranted::new(
                Vec::new(),
                scope_type,
            )));
        }

        let principal_type = Self::map_subject_type(request.subject.subject_type.as_deref())?;
        // RBAC's `evaluate_permission` (unlike `get_subject_roles`) does NOT
        // collapse `Scope::Root` to the caller's `subject_tenant_id`, so
        // evaluating a tenant subject at the literal platform root would deny
        // every tenant-scoped grant. When the request carries no explicit
        // eval-tenant (`tenant_context.root_id`), fall back to the subject's
        // home tenant so a tenant member is evaluated within its own tenant.
        let context_scope = match evaluation_context_to_scope(&request.context) {
            Scope::Root => subject_home_tenant(request)?.map_or_else(Scope::root, Scope::tenant),
            scoped => scoped,
        };

        // Envelope-only debug event. Never the `subject_id`: it is a principal
        // identifier, and emitting it on every authorization check would leak
        // caller identity into general log sinks, below the audit level
        // (DESIGN.md p1 `cpt-cf-authz-plugin-principle-no-sensitive-logs`).
        // Correlate via the request trace_id the canonical middleware injects;
        // the `cf-authz.audit` record carries the subject when auditing is on.
        debug!(
            ?principal_type,
            action = %request.action.name,
            resource_type = %request.resource.resource_type,
            "policy evaluation"
        );

        let rbac_request = EvaluatePermissionRequest::new(
            request.subject.id.to_string(),
            principal_type,
            canonicalize_operation(&request.action.name),
            context_scope,
            &request.resource.resource_type,
        );

        // S2S caller context (TODO #1597, in-process path): the plugin is
        // trusted first-party PDP infrastructure, so it presents a first-party
        // Root caller (`token_scopes = ["*"]`) — the RBAC caller-gate then
        // admits evaluation at any scope. `subject_tenant_id` carries the
        // subject's home tenant: the caller-gate rejects a nil tenant, and it
        // is the eval-tenant fallback for `get_subject_roles`. The eval tenant
        // for `evaluate_permission` is set explicitly in `context_scope` above,
        // because that path does NOT derive it from `subject_tenant_id`.
        let rbac_ctx = build_rbac_ctx(request)?;
        let response = match self.rbac.evaluate_permission(&rbac_ctx, rbac_request).await {
            Ok(response) => response,
            // `AuthorizationDenied` is a 403-class authorization failure, not an
            // outage. Surfacing it as a business deny (not a 503) keeps the
            // deny taxonomy honest — masking it as ServiceUnavailable would
            // read as a phantom RBAC outage. Every other RBAC error is a
            // system/transport fault and stays a fail-closed 503.
            Err(rbac_sdk::error::RbacServiceError::AuthorizationDenied { message }) => {
                warn!(%message, "rbac authorization denied -> business deny");
                let response = build_deny_response(
                    error_codes::INSUFFICIENT_PERMISSIONS_V1,
                    Some(format!(
                        "authorization denied for operation '{}' on resource type '{}'",
                        request.action.name, request.resource.resource_type
                    )),
                );
                return Ok(PolicyOutcome::Denied(response));
            }
            Err(rbac_err) => {
                warn!(error = ?rbac_err, "rbac service unavailable");
                return Err(PluginError::RbacUnavailable);
            }
        };

        match response.result {
            PermissionResult::Allowed(granted) => {
                debug!(outcome = "allowed", "policy evaluation result");
                Ok(PolicyOutcome::Allowed(granted))
            }
            PermissionResult::Denied(_denied) => {
                debug!(outcome = "denied", "policy evaluation result");
                let response = build_deny_response(
                    error_codes::INSUFFICIENT_PERMISSIONS_V1,
                    Some(format!(
                        "subject lacks permission '{}' on resource type '{}'",
                        request.action.name, request.resource.resource_type
                    )),
                );
                Ok(PolicyOutcome::Denied(response))
            }
            // `PermissionResult` is `#[non_exhaustive]`. Treat any
            // future-added variant as a fail-closed deny — same as the
            // SDK doc on `PermissionScopeType` instructs for reserved
            // scope variants.
            other => {
                // Surface SDK drift at warn — an unrecognized variant means
                // the RBAC SDK added a `PermissionResult` case this plugin
                // doesn't handle yet. Fail closed, but make it visible.
                // `result_debug` includes the variant name so the new case is
                // identifiable from logs — `PermissionResult` carries grants
                // and scope, never tokens, so this is safe to log.
                warn!(
                    outcome = "denied",
                    reason = "unknown_permission_result_variant",
                    result_debug = ?other,
                    "policy evaluation result: unrecognized RBAC PermissionResult variant"
                );
                let response = build_deny_response(
                    error_codes::INSUFFICIENT_PERMISSIONS_V1,
                    Some(format!(
                        "rbac returned an unrecognized PermissionResult variant for operation \
                         '{}' on resource type '{}'",
                        request.action.name, request.resource.resource_type
                    )),
                );
                Ok(PolicyOutcome::Denied(response))
            }
        }
    }
}

/// Translate the `AuthZ` SDK's `EvaluationRequestContext` into an RBAC
/// `Scope`. Pure function. `Scope::Root` is the RBAC default — the SDK doc
/// on `context_scope` notes it resolves to the caller's home tenant inside
/// the evaluator.
pub(crate) fn evaluation_context_to_scope(ctx: &EvaluationRequestContext) -> Scope {
    match ctx.tenant_context.as_ref().and_then(|tc| tc.root_id) {
        Some(tenant_id) => Scope::tenant(tenant_id),
        None => Scope::root(),
    }
}

/// First-party / unrestricted token-scope marker. A caller presenting
/// `token_scopes = ["*"]` is treated by the RBAC caller-gate as a first-party
/// Root caller, which may address any scope.
const FIRST_PARTY_ROOT_SCOPE: &str = "*";

/// Build the S2S caller context for the in-process RBAC call.
///
/// The plugin is trusted first-party PDP infrastructure (design §3.8: Plugin →
/// RBAC is implicit in-process trust), so it presents a first-party Root caller
/// (`token_scopes = ["*"]`); the RBAC caller-gate then admits evaluation at any
/// scope. `subject_tenant_id` carries the subject's home tenant: the caller-gate
/// rejects a nil tenant, and `get_subject_roles` uses it as its `Scope::Root`
/// eval-tenant fallback. `evaluate_permission` does NOT apply that fallback, so
/// `evaluate_permissions` sets the eval tenant explicitly in the request scope.
/// When the home tenant is absent we fall back to the request's `root_id`; if
/// neither is present the caller context cannot be built and we fail closed
/// with a system error.
pub(crate) fn build_rbac_ctx(
    request: &EvaluationRequest,
) -> Result<toolkit_security::SecurityContext, PluginError> {
    let home_tenant = subject_home_tenant(request)?
        .or_else(|| {
            request
                .context
                .tenant_context
                .as_ref()
                .and_then(|tc| tc.root_id)
        })
        .ok_or_else(|| {
            PluginError::internal(
                "cannot build RBAC caller context: request carries no subject tenant_id and no \
                 tenant_context.root_id",
            )
        })?;
    toolkit_security::SecurityContext::builder()
        .subject_id(request.subject.id)
        .subject_tenant_id(home_tenant)
        .token_scopes(vec![FIRST_PARTY_ROOT_SCOPE.to_owned()])
        .build()
        .map_err(|err| PluginError::internal(format!("failed to build RBAC caller context: {err}")))
}

/// Extract the subject's home tenant from `subject.properties["tenant_id"]`
/// (the `AuthZEN` convention also used by the static plugin and `hierarchy_client`).
///
/// `Ok(None)` means the claim is **absent**, which [`build_rbac_ctx`]
/// answers with the documented `tenant_context.root_id` fallback.
///
/// A claim that is present but unparseable is an error, not an absence.
/// Collapsing the two with `.ok()` sent a malformed `tenant_id` down that
/// same fallback, so the caller context was built with the *root* tenant as
/// the subject's home tenant — which RBAC uses as its `Scope::Root`
/// eval-tenant and its group-membership fallback. Fail closed instead: a
/// request whose own tenant claim we cannot read is not a request we can
/// scope.
fn subject_home_tenant(request: &EvaluationRequest) -> Result<Option<Uuid>, PluginError> {
    let Some(raw) = request.subject.properties.get("tenant_id") else {
        return Ok(None);
    };
    let Some(text) = raw.as_str() else {
        return Err(PluginError::UnreadableSubjectTenant {
            detail: "not a string".to_owned(),
        });
    };
    Uuid::parse_str(text).map(Some).map_err(|err| {
        // The value itself is caller-supplied and may be hostile; report the
        // parse failure without echoing it back.
        PluginError::UnreadableSubjectTenant {
            detail: format!("not a UUID: {err}"),
        }
    })
}

/// Canonicalize the `AuthZEN` `action.name` to the RBAC short verb before
/// querying RBAC. `get`/`list` are read-style aliases that role grants express
/// as `read`; every other verb (`read`, `write`, `delete`, `start`, ...) is
/// already canonical and passes through unchanged. Distinct from the OAuth
/// scope-class map in `config.operation_to_scope` (which collapses
/// `delete` → `write`) — RBAC keeps `delete` as its own verb, so that map must
/// NOT be reused here.
pub(crate) fn canonicalize_operation(action_name: &str) -> &str {
    match action_name {
        "get" | "list" => "read",
        other => other,
    }
}

#[cfg(test)]
#[path = "policy_evaluator_tests.rs"]
mod tests;
