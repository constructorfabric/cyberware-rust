//! Minimal allow-with-owner-constraints PDP mock for integration tests.
//!
//! Integration binaries cannot see the crate-internal `#[cfg(test)]`
//! `test_support` harness, so this mirrors its `MockAuthZResolver` +
//! `enforcer_allow()` over the public `authz-resolver-sdk` surface: every
//! decision is a positive allow carrying owner constraints (`owner_tenant_id`
//! == subject tenant AND `owner_id` == subject id), so scoped reads/writes
//! filter to the caller's own rows.
//
// @cpt-cf-chat-engine-design-auth-model

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::pep::PolicyEnforcer;
use authz_resolver_sdk::{
    AuthZResolverApi,
    constraints::{Constraint, InPredicate, Predicate},
    models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext},
};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::{PlatformSecurityContext, pep_properties};
use uuid::Uuid;

fn resolve_subject_tenant(request: &EvaluationRequest) -> Option<Uuid> {
    request
        .context
        .tenant_context
        .as_ref()
        .and_then(|tc| tc.root_id)
        .or_else(|| {
            request
                .subject
                .properties
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok())
        })
        .filter(|id| !id.is_nil())
}

struct MockAuthZResolver;

#[async_trait]
impl AuthZResolverApi for MockAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let subject_id = request.subject.id;
        let constraints = match resolve_subject_tenant(&request) {
            Some(tenant) => vec![Constraint {
                predicates: vec![
                    Predicate::In(InPredicate::new(pep_properties::OWNER_TENANT_ID, [tenant])),
                    Predicate::In(InPredicate::new(pep_properties::OWNER_ID, [subject_id])),
                ],
            }],
            None => vec![],
        };
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                ..Default::default()
            },
        })
    }
}

/// Allow-with-owner-constraints enforcer (integration happy path).
#[must_use]
pub fn enforcer_allow() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(MockAuthZResolver))
}

/// PDP that constrains the TENANT only — a faithful model of the shipped
/// policy plugins (`static-authz`, `tr-authz`), neither of which emits an
/// `owner_id` predicate. Under this PDP the compiled scope admits every row in
/// the caller's tenant, so it is the fixture that shows whether the gear's own
/// ownership enforcement is doing the work.
//
// @cpt-cf-chat-engine-nfr-authentication
struct TenantOnlyAuthZResolver;

#[async_trait]
impl AuthZResolverApi for TenantOnlyAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let constraints = match resolve_subject_tenant(&request) {
            Some(tenant) => vec![Constraint {
                predicates: vec![Predicate::In(InPredicate::new(
                    pep_properties::OWNER_TENANT_ID,
                    [tenant],
                ))],
            }],
            None => vec![],
        };
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints,
                ..Default::default()
            },
        })
    }
}

/// Tenant-only enforcer — models the shipped policy plugins.
#[must_use]
pub fn enforcer_allow_tenant_only() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(TenantOnlyAuthZResolver))
}

/// PDP that denies every request — models a real policy rejecting a subject
/// (e.g. a cross-tenant caller). Every decision fails closed to `Forbidden`.
struct DenyAllAuthZResolver;

#[async_trait]
impl AuthZResolverApi for DenyAllAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// Deny-all enforcer — every decision is `Forbidden` (403).
#[must_use]
pub fn enforcer_deny() -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(DenyAllAuthZResolver))
}
