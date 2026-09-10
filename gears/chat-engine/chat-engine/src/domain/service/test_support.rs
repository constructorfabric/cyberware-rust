//! Shared test harness for the domain-service authorization suite (Phase 8).
//!
//! Provides mock `AuthZResolverApi` implementations (mirroring the
//! `users-info` reference gear), `PolicyEnforcer` builders, `SecurityContext`
//! fixtures, an in-memory SQLite database with the real migrations applied,
//! Sea-ORM repo builders, and row-seed helpers. Real repos + a mock PDP let the
//! tests exercise the full PEP → `AccessScope` → SecureORM `WHERE` flow.
//!
//! chat-engine differs from `users-info` on one point: the [`MockAuthZResolver`]
//! returns owner constraints on BOTH the constrained and unconstrained paths, so
//! a cross-tenant point-op resolves to a 0-row scoped fetch → `NotFound` (404,
//! anti-enumeration) rather than a PDP `Denied` (403). See DESIGN §3.5.5.
//
// @cpt-cf-chat-engine-design-auth-model
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::pep::PolicyEnforcer;
use authz_resolver_sdk::{
    AuthZResolverApi,
    constraints::{Constraint, InPredicate, Predicate},
    models::{EvaluationRequest, EvaluationResponse, EvaluationResponseContext},
};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use toolkit::ClientHub;

use crate::domain::ports::{MessageRepo, NewSession, ReactionRepo, SessionRepo, SessionTypeRepo};
use crate::domain::service::message_service::MessageService;
use crate::domain::service::plugin_service::PluginService;
use crate::domain::service::reaction_service::ReactionService;
use crate::domain::service::session_service::SessionService;
use crate::domain::service::variant_service::{VariantRepo, VariantService};
use crate::domain::service::webhook::NoopWebhookEmitter;
use crate::domain::session::Session;
use crate::infra::db::migrations::Migrator;
use crate::infra::db::repo::ChatEngineDb;
use crate::infra::db::repo::message_repo::SeaMessageRepo;
use crate::infra::db::repo::plugin_config_repo::SeaPluginConfigRepo;
use crate::infra::db::repo::reaction_repo::SeaReactionRepo;
use crate::infra::db::repo::session_repo::SeaSessionRepo;
use crate::infra::db::repo::session_type_repo::SeaSessionTypeRepo;
use crate::infra::db::repo::variant_repo::SeaVariantRepo;

// ---------------------------------------------------------------------------
// SecurityContext fixtures
// ---------------------------------------------------------------------------

/// Context for an authenticated subject in the first of `tenants` (fresh random
/// `subject_id`). Mirrors the reference gear's `ctx_allow_tenants`.
#[must_use]
pub fn ctx_allow_tenants(tenants: &[Uuid]) -> SecurityContext {
    let tenant_id = tenants.first().copied().unwrap_or_else(Uuid::new_v4);
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

/// Context for a specific `(subject_id, tenant_id)` — owner-based tests where
/// `subject_id` must equal the row's `owner_id`.
#[must_use]
pub fn ctx_for_subject(subject_id: Uuid, tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject_id)
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

// ---------------------------------------------------------------------------
// In-memory database + repo builders
// ---------------------------------------------------------------------------

/// Fresh in-memory SQLite database with the real chat-engine migrations applied.
/// One connection (SQLite `:memory:` is per-connection) so every repo shares the
/// same schema+data. ~1 ms per call.
pub async fn inmem_db() -> Arc<ChatEngineDb> {
    let opts = ConnectOpts {
        max_conns: Some(1),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = connect_db("sqlite::memory:", opts)
        .await
        .expect("failed to connect to in-memory database");
    run_migrations_for_testing(&db, Migrator::migrations())
        .await
        .map_err(|e| e.to_string())
        .expect("failed to run chat-engine migrations on in-memory database");
    Arc::new(DBProvider::new(db))
}

#[must_use]
pub fn session_repo(db: &Arc<ChatEngineDb>) -> Arc<dyn SessionRepo> {
    Arc::new(SeaSessionRepo::new(Arc::clone(db)))
}

#[must_use]
pub fn message_repo(db: &Arc<ChatEngineDb>) -> Arc<dyn MessageRepo> {
    Arc::new(SeaMessageRepo::new(Arc::clone(db)))
}

#[must_use]
pub fn reaction_repo(db: &Arc<ChatEngineDb>) -> Arc<dyn ReactionRepo> {
    Arc::new(SeaReactionRepo::new(Arc::clone(db)))
}

#[must_use]
pub fn session_type_repo(db: &Arc<ChatEngineDb>) -> Arc<dyn SessionTypeRepo> {
    Arc::new(SeaSessionTypeRepo::new(Arc::clone(db)))
}

#[must_use]
pub fn variant_repo(db: &Arc<ChatEngineDb>) -> Arc<dyn VariantRepo> {
    Arc::new(SeaVariantRepo::new(Arc::clone(db)))
}

// ---------------------------------------------------------------------------
// Service builders (real repos + inmem DB + injected enforcer)
// ---------------------------------------------------------------------------

/// Empty plugin service (real `SeaPluginConfigRepo`, empty `ClientHub`). The
/// authz tests deny/allow before any plugin call, so no plugin needs to be
/// registered.
#[must_use]
fn test_plugins(db: &Arc<ChatEngineDb>) -> PluginService {
    let hub = Arc::new(ClientHub::new());
    PluginService::new(hub, Arc::new(SeaPluginConfigRepo::new(Arc::clone(db))))
}

#[must_use]
pub fn build_session_service(db: &Arc<ChatEngineDb>, enforcer: PolicyEnforcer) -> SessionService {
    SessionService::new(
        session_repo(db),
        session_type_repo(db),
        test_plugins(db),
        Arc::new(NoopWebhookEmitter),
        enforcer,
    )
}

#[must_use]
pub fn build_message_service(db: &Arc<ChatEngineDb>, enforcer: PolicyEnforcer) -> MessageService {
    MessageService::new(
        session_repo(db),
        session_type_repo(db),
        message_repo(db),
        test_plugins(db),
        enforcer,
    )
}

#[must_use]
pub fn build_reaction_service(db: &Arc<ChatEngineDb>, enforcer: PolicyEnforcer) -> ReactionService {
    ReactionService::new(
        session_repo(db),
        session_type_repo(db),
        message_repo(db),
        reaction_repo(db),
        test_plugins(db),
        enforcer,
    )
}

#[must_use]
pub fn build_variant_service(db: &Arc<ChatEngineDb>, enforcer: PolicyEnforcer) -> VariantService {
    let message_service = Arc::new(build_message_service(db, enforcer.clone()));
    VariantService::new(
        session_repo(db),
        session_type_repo(db),
        message_repo(db),
        variant_repo(db),
        test_plugins(db),
        message_service,
        enforcer,
    )
}

// ---------------------------------------------------------------------------
// Row-seed helpers (bypass the service layer)
// ---------------------------------------------------------------------------

/// Seed a session row directly (owner pair = `tenant_id` / `user_id`) via the
/// repo's scoped insert. Uses a real tenant-scoped `AccessScope` (never
/// `allow_all()`), so the bypass-registry grep gate stays clean.
pub async fn seed_session(
    db: &Arc<ChatEngineDb>,
    session_id: Uuid,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Session {
    let scope = AccessScope::for_tenant(tenant_id);
    let now = OffsetDateTime::now_utc();
    session_repo(db)
        .insert_scoped(
            &scope,
            NewSession {
                session_id,
                tenant_id: tenant_id.to_string(),
                user_id: user_id.to_string(),
                client_id: None,
                session_type_id: None,
                metadata: None,
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .expect("failed to seed session")
}

// ---------------------------------------------------------------------------
// PolicyEnforcer builders
// ---------------------------------------------------------------------------

/// Build a [`PolicyEnforcer`] over the given resolver.
#[must_use]
pub fn enforcer(resolver: Arc<dyn AuthZResolverApi>) -> PolicyEnforcer {
    PolicyEnforcer::new(resolver)
}

/// Allow-with-owner-constraints enforcer (the standard happy path).
#[must_use]
pub fn enforcer_allow() -> PolicyEnforcer {
    enforcer(Arc::new(MockAuthZResolver))
}

/// Deny-all enforcer — every decision is `false` → `Forbidden` (403).
#[must_use]
pub fn enforcer_deny() -> PolicyEnforcer {
    enforcer(Arc::new(DenyAllAuthZResolver))
}

/// Tenant-only enforcer — models the shipped policy plugins, which constrain
/// `owner_tenant_id` and never `owner_id`.
#[must_use]
pub fn enforcer_allow_tenant_only() -> PolicyEnforcer {
    enforcer(Arc::new(TenantOnlyAuthZResolver))
}

/// Allow enforcer with NO ABAC constraints — models a pure permission gate
/// (e.g. `SESSION_TYPE`, DESIGN §3.5.6) where the PDP returns `decision=true`
/// and no owner constraints, so nothing needs to compile against a scopable
/// resource. Use this for the global-table surfaces `MockAuthZResolver`'s
/// owner constraints would fail-closed against.
#[must_use]
pub fn enforcer_allow_unconstrained() -> PolicyEnforcer {
    enforcer(Arc::new(AllowUnconstrainedAuthZResolver))
}

/// PDP-unreachable enforcer — `EvaluationFailed` → `Forbidden` (403, fail-closed).
#[must_use]
pub fn enforcer_failing() -> PolicyEnforcer {
    enforcer(Arc::new(FailingAuthZResolver))
}

/// Allow-but-no-constraints enforcer — `CompileFailed` on constrained calls →
/// `Forbidden` (403, fail-closed).
#[must_use]
pub fn enforcer_compile_fail() -> PolicyEnforcer {
    enforcer(Arc::new(CompileFailAuthZResolver))
}

// ---------------------------------------------------------------------------
// Mock AuthZ resolvers
// ---------------------------------------------------------------------------

/// Resolve the subject's effective tenant like a real PDP: explicit tenant
/// context first, then the `tenant_id` subject property; nil UUIDs are treated
/// as "unset" (anonymous/root).
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

/// Allow resolver that emits owner constraints (`owner_tenant_id` == subject
/// tenant AND `owner_id` == subject id) on EVERY positive decision — including
/// the `require_constraints=false` point-op path. That forces the point-op's
/// scoped re-read to filter by owner, so a cross-tenant/cross-owner target
/// resolves to 0 rows → `NotFound` (404) instead of leaking via the prefetch
/// fast-path. `SESSION_TYPE` (no supported properties) drops these constraints
/// at compile time, leaving a plain allow decision.
//
// @cpt-cf-chat-engine-design-auth-model
pub struct MockAuthZResolver;

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

/// Allow resolver that constrains the TENANT only — a faithful model of the
/// shipped policy plugins (`static-authz`, `tr-authz`), neither of which emits
/// an `owner_id` predicate. Under this PDP the compiled scope admits every row
/// in the caller's tenant, so it is the fixture that proves the gear's own
/// ownership guard (`owner_guard::ensure_session_owner`) is what keeps one
/// user out of another user's session.
//
// @cpt-cf-chat-engine-nfr-authentication
pub struct TenantOnlyAuthZResolver;

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

/// Allow resolver that returns `decision=true` with NO constraints — the
/// accurate model for a global-table permission gate (`SESSION_TYPE`): a plain
/// allow with nothing to compile against a scopable resource.
//
// @cpt-cf-chat-engine-design-auth-model
pub struct AllowUnconstrainedAuthZResolver;

#[async_trait]
impl AuthZResolverApi for AllowUnconstrainedAuthZResolver {
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

/// PDP that explicitly denies every request (`decision=false`) →
/// `EnforcerError::Denied` → `ChatEngineError::Forbidden`.
//
// @cpt-cf-chat-engine-design-auth-model
pub struct DenyAllAuthZResolver;

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

/// PDP that is unreachable — returns `AuthZResolverError::Internal` →
/// `EnforcerError::EvaluationFailed` → `ChatEngineError::Forbidden` (fail-closed,
/// never 503).
//
// @cpt-cf-chat-engine-constraint-fail-closed-authz
pub struct FailingAuthZResolver;

#[async_trait]
impl AuthZResolverApi for FailingAuthZResolver {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Err(CanonicalError::internal("PDP unavailable".to_owned()).create())
    }
}

/// PDP that allows but returns no constraints. On a `require_constraints=true`
/// call the enforcer cannot compile a scope → `EnforcerError::CompileFailed` →
/// `ChatEngineError::Forbidden` (fail-closed).
//
// @cpt-cf-chat-engine-constraint-fail-closed-authz
pub struct CompileFailAuthZResolver;

#[async_trait]
impl AuthZResolverApi for CompileFailAuthZResolver {
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
