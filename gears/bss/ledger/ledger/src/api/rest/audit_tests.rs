//! Unit tests for the pure parts of the audit-retrieval surface: the DTO
//! projections. The handlers read through a `DBProvider`, so the end-to-end
//! audit-retrieval + tamper-status behavior (who/when/source dims of a posted
//! entry; tamper-status reflecting an inserted freeze) is exercised against a
//! real database in `tests/postgres_cross_tenant.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::HeaderMap;
use uuid::Uuid;

use crate::api::rest::audit::correlation_id_header;
use crate::api::rest::dto::{AuditEntryDto, AuditPackExportDto, FreezeDto, TamperStatusDto};
use crate::infra::audit::retrieval::{AuditEntryRecord, FreezeRecord, TamperStatusRecord};
use crate::infra::storage::entity::audit_pack_export;
use time::OffsetDateTime;

/// S-1: a valid `X-Correlation-Id` header is parsed into the audit record's
/// `correlation_id`; absent or unparseable yields `None` (the record is still
/// written, just without the cross-trace key).
#[test]
fn correlation_id_header_parses_uuid_or_none() {
    let id = Uuid::now_v7();

    let mut valid = HeaderMap::new();
    valid.insert("X-Correlation-Id", id.to_string().parse().unwrap());
    assert_eq!(
        correlation_id_header(&valid),
        Some(id),
        "valid UUID header → Some(id)"
    );

    assert_eq!(
        correlation_id_header(&HeaderMap::new()),
        None,
        "absent header → None"
    );

    let mut garbage = HeaderMap::new();
    garbage.insert("X-Correlation-Id", "not-a-uuid".parse().unwrap());
    assert_eq!(
        correlation_id_header(&garbage),
        None,
        "unparseable header → None"
    );
}

#[test]
fn audit_entry_dto_carries_who_when_source_correlation() {
    let actor = Uuid::now_v7();
    let corr = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let record = AuditEntryRecord {
        entry_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        period_id: "202606".to_owned(),
        posted_by_actor_id: actor,
        origin: "API".to_owned(),
        posted_at_utc: now,
        source_doc_type: "INVOICE_POST".to_owned(),
        source_business_id: "INV-1".to_owned(),
        correlation_id: corr,
        reverses_entry_id: None,
        created_seq: 7,
    };
    let dto = AuditEntryDto::from(record);
    assert_eq!(dto.posted_by_actor_id, actor);
    assert_eq!(dto.correlation_id, corr);
    assert_eq!(dto.origin, "API");
    assert_eq!(dto.source_doc_type, "INVOICE_POST");
    assert_eq!(dto.source_business_id, "INV-1");
    assert_eq!(dto.posted_at_utc, now);
}

#[test]
fn tamper_status_dto_reflects_active_freeze() {
    let now = OffsetDateTime::now_utc();
    let record = TamperStatusRecord {
        scope_frozen: true,
        freezes: vec![FreezeRecord {
            scope: "tenant".to_owned(),
            period_id: "ALL".to_owned(),
            reason: "broken chain".to_owned(),
            frozen_at: now,
            set_by: "verifier".to_owned(),
            cleared_by: None,
            cleared_at: None,
        }],
        verified: false,
    };
    let dto = TamperStatusDto::from(record);
    assert!(
        dto.scope_frozen,
        "an active freeze must surface scope_frozen"
    );
    assert!(!dto.verified, "a frozen scope derives verified=false");
    assert_eq!(dto.freezes.len(), 1);
    let f: &FreezeDto = &dto.freezes[0];
    assert_eq!(f.reason, "broken chain");
    assert!(f.cleared_at.is_none(), "an active freeze has no cleared_at");
}

#[test]
fn tamper_status_dto_unfrozen_is_verified() {
    let record = TamperStatusRecord {
        scope_frozen: false,
        freezes: Vec::new(),
        verified: true,
    };
    let dto = TamperStatusDto::from(record);
    assert!(!dto.scope_frozen);
    assert!(
        dto.verified,
        "an unfrozen scope derives verified=true (MVP)"
    );
    assert!(dto.freezes.is_empty());
}

/// Build a succeeded export model for the DTO-projection tests.
fn succeeded_export(csv: &str) -> audit_pack_export::Model {
    let now = OffsetDateTime::now_utc();
    audit_pack_export::Model {
        export_id: Uuid::now_v7(),
        tenant_id: Uuid::now_v7(),
        target_tenant_id: Uuid::now_v7(),
        status: "succeeded".to_owned(),
        reason_code: Some("DISPUTE_INVESTIGATION".to_owned()),
        actor_ref: Uuid::now_v7().to_string(),
        csv: Some(csv.as_bytes().to_vec()),
        row_count: 3,
        error_detail: None,
        created_at_utc: now,
        completed_at_utc: Some(now),
    }
}

/// The full (polled) projection decodes the stored CSV bytes and carries every
/// job field.
#[test]
fn audit_pack_export_dto_from_model_includes_csv() {
    let model = succeeded_export("h1,h2\na,b\n");
    let export_id = model.export_id;
    let target = model.target_tenant_id;
    let dto = AuditPackExportDto::from(model);
    assert_eq!(dto.export_id, export_id);
    assert_eq!(dto.status, "succeeded");
    assert_eq!(dto.target_tenant_id, target);
    assert_eq!(dto.row_count, 3);
    assert_eq!(dto.csv.as_deref(), Some("h1,h2\na,b\n"));
    assert!(dto.completed_at_utc.is_some());
}

/// The 202-create summary carries the job identity + state but NOT the CSV body
/// (the client polls the Location for the materialized pack).
#[test]
fn audit_pack_export_dto_summary_omits_csv() {
    let model = succeeded_export("h1,h2\na,b\n");
    let dto = AuditPackExportDto::summary(&model);
    assert_eq!(dto.export_id, model.export_id);
    assert_eq!(dto.status, "succeeded");
    assert_eq!(dto.row_count, 3);
    assert!(dto.csv.is_none(), "the 202 summary must omit the CSV body");
}

// ── cross_tenant_role_authorized — the cross-tenant elevation authorization ──
//
// These pin the fix for the BOLA where the cross-tenant audit read trusted a
// hardcoded `role_authorized = true`: opening a tenant other than the caller's
// home now runs a target-anchored PEP decision, and only an authorized target
// elevates.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, InPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_gts::gts_id;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};

use crate::infra::authz::cross_tenant::TargetScope;

/// Degraded flat-`In` PDP fake authorizing exactly one tenant (mirrors
/// `authz_tests::FlatInResolver` — the shape the production PDP returns for a
/// PEP advertising no subtree capability).
struct FlatInResolver {
    allowed: Uuid,
}

#[async_trait]
impl AuthZResolverApi for FlatInResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::In(InPredicate::new(
                        pep_properties::OWNER_TENANT_ID,
                        vec![self.allowed],
                    ))],
                }],
                deny_reason: None,
            },
        })
    }
}

/// PDP fake that always fails to evaluate (models an unreachable PDP).
struct FailingResolver;

#[async_trait]
impl AuthZResolverApi for FailingResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::internal("pdp unreachable".to_owned()).create())
    }
}

fn flat_in_enforcer(allowed: Uuid) -> PolicyEnforcer {
    PolicyEnforcer::new(Arc::new(FlatInResolver { allowed }))
}

fn ctx_for(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::now_v7())
        .subject_tenant_id(tenant)
        .subject_type(gts_id!("cf.core.security.subject_user.v1~"))
        .token_scopes(vec!["*".to_owned()])
        .build()
        .expect("authed SecurityContext must build")
}

/// A cross-tenant target INSIDE the caller's authorized scope elevates
/// (`Ok(true)`).
#[tokio::test]
async fn cross_tenant_authorized_when_target_in_scope() {
    let home = Uuid::now_v7();
    let target = Uuid::now_v7();
    // The caller is authorized for the TARGET tenant (e.g. a self-managed child).
    let enforcer = flat_in_enforcer(target);
    let ctx = ctx_for(home);
    let ok = super::cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home,
        Some(TargetScope { tenant_id: target }),
        crate::authz::actions::AUDIT_READ,
    )
    .await
    .expect("an authorized target must not error");
    assert!(ok, "a target inside the caller's scope must elevate");
}

/// A cross-tenant target OUTSIDE the caller's authorized scope is denied
/// (`Ok(false)` → `CROSS_TENANT_ACCESS_DENIED` in the gateway). This is the
/// BOLA regression guard.
#[tokio::test]
async fn cross_tenant_denied_when_target_outside_scope() {
    let home = Uuid::now_v7();
    let target = Uuid::now_v7();
    // The caller is authorized for its HOME tenant only — not the target.
    let enforcer = flat_in_enforcer(home);
    let ctx = ctx_for(home);
    let ok = super::cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home,
        Some(TargetScope { tenant_id: target }),
        crate::authz::actions::AUDIT_READ,
    )
    .await
    .expect("a PDP deny is Ok(false), not an error");
    assert!(
        !ok,
        "a target outside the caller's scope must NOT elevate (BOLA guard)"
    );
}

/// The routine path (no target, or the target IS the home tenant) returns
/// `Ok(true)` WITHOUT calling the PDP — proven by a resolver that would error if
/// consulted.
#[tokio::test]
async fn routine_home_target_skips_pdp() {
    let home = Uuid::now_v7();
    let enforcer = PolicyEnforcer::new(Arc::new(FailingResolver));
    let ctx = ctx_for(home);

    let no_target = super::cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home,
        None,
        crate::authz::actions::AUDIT_READ,
    )
    .await
    .expect("the no-target path must not consult the PDP");
    assert!(no_target, "no target is a routine read");

    let self_target = super::cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home,
        Some(TargetScope { tenant_id: home }),
        crate::authz::actions::AUDIT_READ,
    )
    .await
    .expect("the home-target path must not consult the PDP");
    assert!(self_target, "target == home is a routine read");
}

/// An unreachable PDP on the cross-tenant path fails closed (propagates an
/// error → 503), never silently elevating.
#[tokio::test]
async fn cross_tenant_pdp_unavailable_propagates_error() {
    let home = Uuid::now_v7();
    let target = Uuid::now_v7();
    let enforcer = PolicyEnforcer::new(Arc::new(FailingResolver));
    let ctx = ctx_for(home);
    let res = super::cross_tenant_role_authorized(
        &enforcer,
        &ctx,
        home,
        Some(TargetScope { tenant_id: target }),
        crate::authz::actions::AUDIT_READ,
    )
    .await;
    assert!(
        res.is_err(),
        "an unreachable PDP must fail closed, got {res:?}"
    );
}

// ── investigation_reason — the single header source for the forensic reason ──

/// The free-text reason is read from the `X-Investigation-Reason` header (the
/// standardized source across all four cross-tenant endpoints).
#[test]
fn investigation_reason_reads_the_header() {
    use axum::http::{HeaderMap, HeaderValue};
    let mut headers = HeaderMap::new();
    headers.insert(
        super::INVESTIGATION_REASON_HEADER,
        HeaderValue::from_static("Dispute #4821 chargeback review"),
    );
    assert_eq!(
        super::investigation_reason(&headers).as_deref(),
        Some("Dispute #4821 chargeback review")
    );
}

/// An absent header yields `None` (the handlers then treat it as an empty
/// reason, which the cross-tenant gate rejects with `MISSING_INVESTIGATION_REASON`).
#[test]
fn investigation_reason_absent_is_none() {
    use axum::http::HeaderMap;
    let headers = HeaderMap::new();
    assert!(super::investigation_reason(&headers).is_none());
}
