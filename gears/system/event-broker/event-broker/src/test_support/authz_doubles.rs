//! Permissive-by-default `AuthZResolverApi` double for
//! `EventBrokerHarness` (`gears-rust#4516`, `eb-authz-enforcement`).
//! Always-allow so existing/new tests that don't care about authz keep
//! passing unmodified; tests that DO care about denial construct their own
//! narrower double instead of reusing this
//! (design.md "Exact double shape ... is an implementation-time detail").
//! Tenant scope is enforced via the same `AuthZResolverApi`/
//! `PolicyEnforcer` seam (`domain/authz.rs::tenant_authorized`), not a
//! separate `tenant-resolver-sdk` double - there is no second client type
//! to fake here.

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverApi, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::PlatformSecurityContext;

/// Grants every `evaluate` call - `IngestServiceImpl`/`DeliveryServiceImpl`'s
/// `PolicyEnforcer::access_scope_with` calls only ever inspect
/// `EvaluationResponse::decision`/`context.deny_reason` (`require_constraints(false)`
/// means `context.constraints` is always empty regardless), so an
/// unconditional allow with an empty context is a faithful "no PEP configured"
/// stand-in.
pub struct AllowAllAuthZ;

#[async_trait]
impl AuthZResolverApi for AllowAllAuthZ {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// Denies `evaluate` whenever `deny_if` returns `true` for the request being
/// checked, otherwise allows - lets a single test deny exactly the specific
/// `event_type:produce`/`topic:consume`/`event_type:consume`/tenant-scope
/// check it cares about (inspect `request.action.name` and
/// `request.resource.properties`) while every other check on the same
/// harness keeps its default allow.
pub struct DenyingAuthZ<F> {
    pub deny_if: F,
}

#[async_trait]
impl<F> AuthZResolverApi for DenyingAuthZ<F>
where
    F: Fn(&EvaluationRequest) -> bool + Send + Sync,
{
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: !(self.deny_if)(&request),
            context: EvaluationResponseContext::default(),
        })
    }
}
