//! Authorized operation admission: the single PDP boundary of the gear.
//!
//! Every operation, from REST or from the in-process SDK client, enters here
//! (`features/foundation.md`, "Authorized Operation Admission"). The order is
//! fixed: public shape check, PDP call through `PolicyEnforcer`, post-permit
//! tenant gate, then the `AccessScope` travels unmodified to the operation
//! handler for `SecureConn`. QE keeps no PDP decision cache.

use std::sync::Arc;

use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use authz_resolver_sdk::{EnforcerError, PolicyEnforcer};
use quota_enforcement_sdk::TenantId;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use super::error::DomainError;
use super::ports::metrics::{DenialReason, QeMetrics};

/// The explicit, caller-supplied target of an operation. Untrusted until the
/// PDP authorizes it for the authenticated service principal.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionTarget {
    /// Target tenant.
    pub tenant_id: TenantId,
    /// Target resource, when the operation names one.
    pub resource_id: Option<Uuid>,
}

impl AdmissionTarget {
    /// A tenant-level target.
    #[must_use]
    pub const fn tenant(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            resource_id: None,
        }
    }

    /// A resource-level target.
    #[must_use]
    pub const fn resource(tenant_id: TenantId, resource_id: Uuid) -> Self {
        Self {
            tenant_id,
            resource_id: Some(resource_id),
        }
    }
}

/// An authorized target and the scope the handler binds through `SecureConn`.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted {
    /// The PDP-authorized tenant.
    pub tenant_id: TenantId,
    /// The `AccessScope` exactly as `PolicyEnforcer` returned it.
    pub access_scope: AccessScope,
}

/// The PEP boundary.
// @cpt-dod:cpt-cf-quota-enforcement-dod-gateway-admission:p1
#[domain_model]
pub struct Admission {
    enforcer: PolicyEnforcer,
    metrics: Arc<dyn QeMetrics>,
}

impl Admission {
    /// Build the boundary around the platform PEP.
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer, metrics: Arc<dyn QeMetrics>) -> Self {
        Self { enforcer, metrics }
    }

    /// Admit `action` on `resource` for the explicit `target`.
    ///
    /// # Errors
    ///
    /// - [`DomainError::InvalidArgument`] on a malformed public target, before
    ///   any PDP call.
    /// - [`DomainError::PdpDenied`] when the PDP denies, when its constraints
    ///   do not compile, or when the permit does not cover the target tenant.
    /// - [`DomainError::PdpUnavailable`] when the PDP cannot be reached.
    // @cpt-flow:cpt-cf-quota-enforcement-flow-authorized-admission:p1
    pub async fn admit(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
        target: AdmissionTarget,
    ) -> Result<Admitted, DomainError> {
        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-request
        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-authn
        // The request arrives from REST or from the in-process client through
        // this one entry. `ctx` carries the service principal the platform
        // `api-gateway` authenticated; `target` stays untrusted request data
        // until the PDP authorizes it.
        let site = LogSite {
            principal: ctx.subject_id(),
            resource: resource.name(),
            action,
        };
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-authn
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-request

        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-shape
        if let Err(err) = validate_target_shape(target) {
            return Err(self.deny(&site, DenialReason::InvalidArgument, err));
        }
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-shape

        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-pdp
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, target.tenant_id.as_uuid())
            .require_constraints(true);
        let outcome = self
            .enforcer
            .access_scope_with(ctx, resource, action, target.resource_id, &request)
            .await;
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-pdp

        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-deny-if
        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-deny
        let access_scope = outcome.map_err(|err| {
            self.deny(&site, denial_reason(&err), DomainError::from_enforcer(err))
        })?;
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-deny
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-deny-if

        // The PDP authorized the complete tuple. Accept `tenant_id` only when
        // the returned scope covers it; a permit that names other tenants is a
        // denial for this target.
        if !scope_admits_tenant(&access_scope, target.tenant_id) {
            let err = DomainError::PdpDenied {
                reason: Some(DomainError::TENANT_OUT_OF_SCOPE.to_owned()),
            };
            return Err(self.deny(&site, DenialReason::PermissionDenied, err));
        }

        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-scope
        // @cpt-begin:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-forward
        Ok(Admitted {
            tenant_id: target.tenant_id,
            access_scope,
        })
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-forward
        // @cpt-end:cpt-cf-quota-enforcement-flow-authorized-admission:p1:inst-adm-scope
    }

    /// Record a denial on the metrics port and in the log, then hand the
    /// error back. Identifiers go to the log only, never to a label.
    fn deny(&self, site: &LogSite<'_>, reason: DenialReason, err: DomainError) -> DomainError {
        self.metrics.record_denial(reason);
        tracing::warn!(
            target: "qe.admission",
            subject_id = %site.principal,
            resource = site.resource,
            action = site.action,
            reason = reason.as_label(),
            error = %err,
            "admission denied"
        );
        err
    }
}

/// Log context of one admission call.
struct LogSite<'a> {
    principal: Uuid,
    resource: &'a str,
    action: &'a str,
}

/// Public shape rules that hold for every target, before the PDP call.
fn validate_target_shape(target: AdmissionTarget) -> Result<(), DomainError> {
    if target.tenant_id.as_uuid().is_nil() {
        return Err(DomainError::InvalidArgument {
            field: "tenant_id",
            reason: "TENANT_ID_REQUIRED",
        });
    }
    if target.resource_id.is_some_and(|id| id.is_nil()) {
        return Err(DomainError::InvalidArgument {
            field: "resource_id",
            reason: "RESOURCE_ID_INVALID",
        });
    }
    Ok(())
}

/// True when the PDP scope covers `tenant`: either the permit is
/// unconstrained or its tenant constraint names the tenant.
fn scope_admits_tenant(scope: &AccessScope, tenant: TenantId) -> bool {
    scope.is_unconstrained()
        || scope.contains_uuid(pep_properties::OWNER_TENANT_ID, tenant.as_uuid())
}

const fn denial_reason(err: &EnforcerError) -> DenialReason {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DenialReason::PermissionDenied
        }
        EnforcerError::EvaluationFailed(_) => DenialReason::PdpUnavailable,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "admission_tests.rs"]
mod admission_tests;
