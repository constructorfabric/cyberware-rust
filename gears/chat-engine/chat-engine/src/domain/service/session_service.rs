//! Session lifecycle service.
//!
//! `SessionService` is the orchestration layer between the REST handlers
//! (Phase 4 handlers + Phase 14 assembly), the SeaORM-backed repositories
//! (`SessionRepo`, `SessionTypeRepo`, `PluginConfigRepo`), and the backend
//! plugin trait `ChatEngineBackendPlugin` (resolved through
//! [`PluginService`]).
//!
//! All mutating methods enforce the lifecycle state machine via
//! [`ensure_can_transition`] — the only sanctioned wrapper around
//! [`LifecycleState::can_transition_to`]. Every plugin invocation builds a
//! [`PluginCallContext`] that carries an explicit `deadline` plus a
//! [`CancellationToken`], honouring the SDK's three-state `remaining()`
//! contract documented on `PluginCallContext::remaining`.
//!
//! Reserved metadata keys (`memory_strategy`, `retention_policy`,
//! `share_expires_at`) are stripped from any outgoing `Session` by
//! [`domain::session::public_metadata`]; client-supplied metadata that
//! contains a reserved key is rejected with `BadRequest`.
//
// @cpt-cf-chat-engine-session-service:p4
// @cpt-cf-chat-engine-adr-session-metadata:p4
// @cpt-cf-chat-engine-adr-session-deletion-strategy:p4

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_odata::{ODataQuery, Page};
use tracing::{info, warn};
use uuid::Uuid;

use chat_engine_sdk::models::{Capability, CapabilityValue, LifecycleState, TenantId, UserId};
use chat_engine_sdk::plugin::{PluginCallContext, SessionPluginCtx};

use crate::domain::authz::{actions, bypass, owner_guard, resource_types};
use crate::domain::error::{ChatEngineError, Result};
use crate::domain::ports::{DEFAULT_SOFT_DELETE_RETENTION_DAYS, NewSession, SessionRepo};
use crate::domain::ports::{NewSessionType, SessionTypeRepo};
use crate::domain::service::plugin_service::PluginService;
use crate::domain::service::webhook::{WebhookEmitter, WebhookEvent};
use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};

use crate::domain::session::{
    RESERVED_METADATA_KEYS, Session, SessionType, ensure_can_transition, public_metadata,
};

/// Default per-call plugin deadline applied when the service has no other
/// signal. Mirrors the PRD §Performance budget for synchronous lifecycle
/// hooks.
pub const DEFAULT_PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// JWT-derived call identity. Constructed at the REST boundary; services
/// MUST NOT accept tenant / user identifiers from any other source (PRD
/// §7 Security, ADR-0017).
#[domain_model]
#[derive(Debug, Clone)]
pub struct Identity {
    /// Tenant id extracted from the bearer token (`subject_tenant_id`).
    pub tenant_id: String,
    /// User / subject id extracted from the bearer token (`subject_id`).
    pub user_id: String,
    /// Optional client id (device / app) — passed through to plugins but not
    /// used for scoping.
    pub client_id: Option<String>,
}

impl Identity {
    /// Convenience constructor; rejects empty tenant or user ids early so
    /// downstream `TenantId::new` / `UserId::new` panics never fire.
    pub fn new(
        tenant_id: impl Into<String>,
        user_id: impl Into<String>,
        client_id: Option<String>,
    ) -> Result<Self> {
        let tenant_id = tenant_id.into();
        let user_id = user_id.into();
        if tenant_id.is_empty() {
            return Err(ChatEngineError::bad_request(
                "tenant_id missing in identity",
            ));
        }
        if user_id.is_empty() {
            return Err(ChatEngineError::bad_request("user_id missing in identity"));
        }
        Ok(Self {
            tenant_id,
            user_id,
            client_id,
        })
    }
}

/// Request body for `POST /session-types`. The handler maps the wire DTO
/// into this struct after stripping any `tenant_id` / `user_id` fields the
/// client may have attempted to send.
#[domain_model]
#[derive(Debug, Clone)]
pub struct RegisterSessionTypeRequest {
    /// Human-readable name (per ADR-0017 — opaque to Chat Engine).
    pub name: String,
    /// GTS plugin instance ID; resolved via [`PluginService::resolve`].
    pub plugin_instance_id: Option<String>,
    /// Optional plugin configuration JSONB persisted via
    /// [`PluginService::load_config`] / [`PluginConfigRepo::upsert`]. The
    /// shape is plugin-defined.
    pub plugin_config: Option<JsonValue>,
}

/// Request body for `POST /sessions`.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    /// Optional session-type binding. `None` is allowed for sessions that
    /// don't need a backend plugin (e.g., read-only export shells); plugin
    /// calls are skipped when this is `None`.
    pub session_type_id: Option<Uuid>,
    /// Client-supplied metadata. Reserved keys are rejected here so they
    /// can't leak into the persisted row through this surface.
    pub metadata: Option<JsonValue>,
}

/// Service-level result of `delete_session` — handlers decide between
/// 200 (Soft) and 204 (Hard) based on this value.
//
// `Soft` carries a full `Session` while `Hard` is a unit; that size skew is
// intentional. The value is returned by-value and immediately matched at the
// handler, and `Soft` is the common outcome — boxing it to satisfy
// `large_enum_variant` would add an allocation on the hot path to shrink the
// rare `Hard` case, a net pessimisation.
#[allow(clippy::large_enum_variant)]
#[domain_model]
#[derive(Debug, Clone)]
pub enum SessionDeleteOutcome {
    Soft { session: Session },
    Hard,
}

/// Orchestration of session lifecycle plus session-type registration.
#[domain_model]
#[derive(Clone)]
pub struct SessionService {
    sessions: Arc<dyn SessionRepo>,
    session_types: Arc<dyn SessionTypeRepo>,
    plugins: PluginService,
    webhooks: Arc<dyn WebhookEmitter>,
    /// Default per-plugin-call deadline; can be overridden per call site via
    /// [`SessionService::with_plugin_timeout`].
    plugin_timeout: Duration,
    enforcer: PolicyEnforcer,
}

impl SessionService {
    #[must_use]
    pub fn new(
        sessions: Arc<dyn SessionRepo>,
        session_types: Arc<dyn SessionTypeRepo>,
        plugins: PluginService,
        webhooks: Arc<dyn WebhookEmitter>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            sessions,
            session_types,
            plugins,
            webhooks,
            plugin_timeout: DEFAULT_PLUGIN_CALL_TIMEOUT,
            enforcer,
        }
    }

    /// Override the default plugin call deadline (mostly useful for tests
    /// and load-shed scenarios). Returns `self` for chained construction.
    #[must_use]
    pub fn with_plugin_timeout(mut self, timeout: Duration) -> Self {
        self.plugin_timeout = timeout;
        self
    }

    // ---------------------------------------------------------------------
    // Session-type registration
    // ---------------------------------------------------------------------

    pub async fn register_session_type(
        &self,
        ctx: &SecurityContext,
        req: RegisterSessionTypeRequest,
    ) -> Result<SessionType> {
        // @cpt-cf-chat-engine-design-auth-model
        // session_type mutation is a PDP permission decision, not a SQL scope:
        // `session_types` is a global unrestricted table (DESIGN §3.5.6). The
        // returned scope is discarded — this is a pure allow/deny gate.
        // Denied / unreachable / uncompilable PDP → 403 (fail-closed).
        self.enforcer
            .access_scope_with(
                ctx,
                &resource_types::SESSION_TYPE,
                actions::CREATE,
                None,
                &AccessRequest::new().require_constraints(false),
            )
            .await?;

        // Opaque tenant/user strings for the plugin call context below.
        let identity = identity_from_ctx(ctx)?;

        if req.name.trim().is_empty() {
            return Err(ChatEngineError::bad_request(
                "session-type name must not be empty",
            ));
        }

        let session_type_id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();

        // Persist the row before reaching out to the plugin so a slow /
        // unhealthy plugin does not block the developer from registering
        // (per §1.5 — health is advisory). The plugin invocation below is
        // best-effort and never rolls back the insert.
        let inserted = self
            .session_types
            .insert(NewSessionType {
                session_type_id,
                name: req.name.clone(),
                plugin_instance_id: req.plugin_instance_id.clone(),
                created_at: now,
                updated_at: now,
            })
            .await?;

        if let Some(plugin_instance_id) = req.plugin_instance_id.as_deref() {
            // Plugin presence is required when an id was supplied — return
            // 404 (mapped from `NotFound`) so the developer can correct it.
            let plugin = self.plugins.resolve(plugin_instance_id)?;

            // Persist plugin_config (optional, plugin-defined shape) keyed by
            // (plugin_instance_id, session_type_id) so `create_session`'s
            // `PluginService::load_config` observes it later. A persistence
            // failure surfaces to the caller rather than silently dropping the
            // config.
            if let Some(cfg) = req.plugin_config.clone() {
                self.plugins
                    .save_config(plugin_instance_id, session_type_id, cfg)
                    .await?;
            }

            // Build a cancellable, deadline-bounded ctx and invoke the
            // plugin. We discard the capability list here — capabilities
            // become real only when a session is created against this
            // type (per §1.4 of the feature spec).
            let cancel = CancellationToken::new();
            let ctx = PluginCallContext {
                request_id: Uuid::new_v4(),
                tenant_id: TenantId::new(identity.tenant_id.as_str()),
                user_id: UserId::new(identity.user_id.as_str()),
                plugin_instance_id: plugin_instance_id.to_string(),
                session_type_id,
                plugin_config: req.plugin_config.clone(),
                enabled_capabilities: None,
                deadline: Some(Instant::now() + self.plugin_timeout),
                cancel: cancel.clone(),
            };
            let session_ctx = SessionPluginCtx {
                session_type_id,
                session_id: None,
                call_ctx: ctx,
            };

            // `on_session_type_configured` is best-effort: invalid input
            // is the only outcome that fails registration (per the spec).
            match self
                .invoke_with_deadline(plugin.on_session_type_configured(session_ctx), &cancel)
                .await
            {
                // `on_session_type_configured` has no session, so any returned
                // metadata is ignored; only the call's success/failure matters.
                Ok(_result) => {
                    info!(
                        plugin_instance_id = %plugin_instance_id,
                        session_type_id = %session_type_id,
                        "session-type configured with plugin"
                    );
                }
                Err(err) => {
                    warn!(
                        plugin_instance_id = %plugin_instance_id,
                        session_type_id = %session_type_id,
                        error = %err,
                        "plugin on_session_type_configured failed \u{2014} registration kept (advisory)"
                    );
                }
            }

            // Best-effort health probe (PluginService::health_probe folds
            // everything except Healthy into WARN and never blocks).
            if let Err(err) = self.plugins.health_probe(plugin_instance_id).await {
                warn!(
                    plugin_instance_id = %plugin_instance_id,
                    error = %err,
                    "plugin health probe failed during session-type registration (advisory)"
                );
            }
        }

        Ok(inserted)
    }

    // session_type reads are `.authenticated()`-only — no PDP call (DESIGN §3.5.6).
    pub async fn list_session_types(&self, _ctx: &SecurityContext) -> Result<Vec<SessionType>> {
        self.session_types.list().await
    }

    // session_type reads are `.authenticated()`-only — no PDP call (DESIGN §3.5.6).
    pub async fn get_session_type(
        &self,
        _ctx: &SecurityContext,
        session_type_id: Uuid,
    ) -> Result<SessionType> {
        let row = self
            .session_types
            .find_by_id(session_type_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("session_type", session_type_id))?;
        Ok(row)
    }

    // ---------------------------------------------------------------------
    // Session lifecycle
    // ---------------------------------------------------------------------

    pub async fn create_session(
        &self,
        ctx: &SecurityContext,
        req: CreateSessionRequest,
    ) -> Result<Session> {
        reject_reserved_metadata(req.metadata.as_ref())?;
        // Opaque tenant/user strings for plugin-call construction only; the
        // authorization boundary is the enforcer / `SecurityContext` below.
        let identity = identity_from_ctx(ctx)?;

        // Resolve session-type (if requested) before any write so we can
        // surface 404 cleanly.
        let session_type = match req.session_type_id {
            Some(id) => Some(
                self.session_types
                    .find_by_id(id)
                    .await?
                    .ok_or_else(|| ChatEngineError::not_found("session_type", id))?,
            ),
            None => None,
        };

        let session_id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();

        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        // CREATE stamps the owner pair (subject tenant/user) as ABAC input so
        // the PDP can bind the row to its creator; the compiled scope is passed
        // to the insert instead of a manual owner filter.
        let scope = self
            .enforcer
            .access_scope_with(
                ctx,
                &resource_types::SESSION,
                actions::CREATE,
                None,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                    .resource_property(pep_properties::OWNER_ID, ctx.subject_id()),
            )
            .await?;

        let inserted = self
            .sessions
            .insert_scoped(
                &scope,
                NewSession {
                    session_id,
                    tenant_id: identity.tenant_id.clone(),
                    user_id: identity.user_id.clone(),
                    client_id: identity.client_id.clone(),
                    session_type_id: req.session_type_id,
                    metadata: req.metadata,
                    created_at: now,
                    updated_at: now,
                },
            )
            .await?;
        let inserted_id = inserted.session_id;

        // Invoke the plugin once a session-type with a bound plugin exists.
        // Plugin errors here are fatal per the feature spec (§Create Session):
        // map to 502 and roll back the session row to avoid orphaning a
        // session against a plugin that refused to accept it.
        let mut enabled_capabilities: Option<JsonValue> = None;
        let mut plugin_metadata: Option<JsonValue> = None;
        if let Some(ref st) = session_type
            && let Some(plugin_instance_id) = st.plugin_instance_id.clone()
        {
            let plugin = self.plugins.resolve(&plugin_instance_id)?;
            let plugin_config = self
                .plugins
                .load_config(&plugin_instance_id, st.session_type_id)
                .await?;

            let cancel = CancellationToken::new();
            let call_ctx = PluginCallContext {
                request_id: Uuid::new_v4(),
                tenant_id: TenantId::new(identity.tenant_id.as_str()),
                user_id: UserId::new(identity.user_id.as_str()),
                plugin_instance_id: plugin_instance_id.clone(),
                session_type_id: st.session_type_id,
                plugin_config,
                enabled_capabilities: None,
                deadline: Some(Instant::now() + self.plugin_timeout),
                cancel: cancel.clone(),
            };
            let session_ctx = SessionPluginCtx {
                session_type_id: st.session_type_id,
                session_id: Some(inserted_id),
                call_ctx,
            };

            match self
                .invoke_with_deadline(plugin.on_session_created(session_ctx), &cancel)
                .await
            {
                Ok(result) => {
                    enabled_capabilities = Some(capabilities_to_json(result.capabilities));
                    plugin_metadata = result.metadata;
                }
                Err(err) => {
                    // Rollback: hard-delete the orphan session row so the
                    // caller can safely retry. If the rollback itself fails
                    // the row is orphaned — surface that as an internal error
                    // (combining both causes) rather than masking it behind
                    // the otherwise-retryable plugin error.
                    if let Err(rollback_err) = self
                        .sessions
                        .hard_delete(&identity.tenant_id, &identity.user_id, inserted_id)
                        .await
                    {
                        warn!(
                            session_id = %inserted_id,
                            plugin_error = %err,
                            rollback_error = %rollback_err,
                            "failed to roll back orphaned session row after plugin rejection",
                        );
                        return Err(ChatEngineError::internal(format!(
                            "session rollback failed after plugin error ({err}); \
                             session {inserted_id} may be orphaned"
                        )));
                    }
                    return Err(err);
                }
            }
        }

        let mut persisted = if let Some(caps) = enabled_capabilities {
            self.sessions
                .update_capabilities_scoped(&scope, inserted_id, Some(caps))
                .await?
        } else {
            inserted
        };

        // Plugin-supplied metadata is merged into the session metadata
        // (object merge; engine-reserved keys are stripped).
        if let Some(plugin_meta) = plugin_metadata {
            let merged = merge_plugin_metadata(persisted.metadata.clone(), plugin_meta);
            persisted = self
                .sessions
                .update_metadata_scoped(&scope, inserted_id, Some(merged))
                .await?;
        }

        let session: Session = persisted;

        // Webhook event — best-effort, never blocks the response.
        self.webhooks
            .emit(WebhookEvent::SessionCreated {
                session_id: session.session_id,
                tenant_id: identity.tenant_id.clone(),
                user_id: identity.user_id.clone(),
                session_type_id: session.session_type_id,
            })
            .await
            .unwrap_or_else(|err| {
                warn!(error = %err, "webhook emit failed for session.created");
            });

        Ok(redact_session(session))
    }

    pub async fn get_session(&self, ctx: &SecurityContext, session_id: Uuid) -> Result<Session> {
        // authorize_session_op returns a scope-validated row (constrained
        // decisions re-read under the compiled scope → NotFound when out of
        // scope), so no additional re-read is needed here.
        let (row, _scope) = self
            .authorize_session_op(ctx, session_id, actions::READ)
            .await?;
        Ok(redact_session(row))
    }

    pub async fn list_sessions(
        &self,
        ctx: &SecurityContext,
        query: &ODataQuery,
    ) -> Result<Page<Session>> {
        // @cpt-cf-chat-engine-seq-authz-list
        // @cpt-cf-chat-engine-interface-pep
        let scope = self
            .enforcer
            .access_scope(ctx, &resource_types::SESSION, actions::LIST, None)
            .await?;
        let page = self.sessions.list_scoped(&scope, query).await?;
        Ok(page.map_items(redact_session))
    }

    pub async fn update_metadata(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        metadata: JsonValue,
    ) -> Result<Session> {
        reject_reserved_metadata(Some(&metadata))?;

        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::UPDATE)
            .await?;
        // Soft-deleted sessions cannot receive metadata writes per the spec.
        let state = row.lifecycle_state;
        if matches!(
            state,
            LifecycleState::SoftDeleted | LifecycleState::HardDeleted
        ) {
            return Err(ChatEngineError::conflict(
                "session is deleted and cannot accept metadata updates",
            ));
        }

        let updated = self
            .sessions
            .update_metadata_scoped(&scope, session_id, Some(metadata))
            .await?;
        Ok(redact_session(updated))
    }

    pub async fn update_capabilities(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        caps: Vec<CapabilityValue>,
    ) -> Result<Session> {
        // Opaque tenant/user strings for plugin-call construction only.
        let identity = identity_from_ctx(ctx)?;
        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::UPDATE)
            .await?;
        let state = row.lifecycle_state;
        if matches!(
            state,
            LifecycleState::SoftDeleted | LifecycleState::HardDeleted
        ) {
            return Err(ChatEngineError::conflict(
                "session is deleted and cannot accept capability updates",
            ));
        }

        let (new_caps_json, plugin_metadata) = self
            .negotiate_capabilities(&identity, &row, session_id, &caps)
            .await?;

        let mut updated = self
            .sessions
            .update_capabilities_scoped(&scope, session_id, Some(new_caps_json))
            .await?;

        // Merge plugin-supplied metadata into the session metadata.
        if let Some(plugin_meta) = plugin_metadata {
            let merged = merge_plugin_metadata(updated.metadata.clone(), plugin_meta);
            updated = self
                .sessions
                .update_metadata_scoped(&scope, session_id, Some(merged))
                .await?;
        }
        Ok(redact_session(updated))
    }

    /// Atomic PATCH path when both `metadata` and `enabled_capabilities` are
    /// supplied. Runs a single UPDATE authorization and persists both columns
    /// in one transactional write, so either both changes commit or neither
    /// does (no partial update). Behavior matches applying `update_metadata`
    /// then `update_capabilities` sequentially — plugin-returned metadata merges
    /// on top of the request metadata — minus the partial-commit window.
    pub async fn update_metadata_and_capabilities(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        metadata: JsonValue,
        caps: Vec<CapabilityValue>,
    ) -> Result<Session> {
        reject_reserved_metadata(Some(&metadata))?;
        // Opaque tenant/user strings for plugin-call construction only.
        let identity = identity_from_ctx(ctx)?;
        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::UPDATE)
            .await?;
        let state = row.lifecycle_state;
        if matches!(
            state,
            LifecycleState::SoftDeleted | LifecycleState::HardDeleted
        ) {
            return Err(ChatEngineError::conflict(
                "session is deleted and cannot accept updates",
            ));
        }

        let (new_caps_json, plugin_metadata) = self
            .negotiate_capabilities(&identity, &row, session_id, &caps)
            .await?;

        // Base = request metadata; plugin-returned metadata (if any) merges on
        // top — mirroring the sequential path where `update_capabilities` merges
        // plugin metadata onto whatever `update_metadata` just wrote.
        let final_metadata = match plugin_metadata {
            Some(plugin_meta) => merge_plugin_metadata(Some(metadata), plugin_meta),
            None => metadata,
        };

        let updated = self
            .sessions
            .update_metadata_and_capabilities_scoped(
                &scope,
                session_id,
                Some(final_metadata),
                Some(new_caps_json),
            )
            .await?;
        Ok(redact_session(updated))
    }

    /// Run the capability-negotiation plugin round-trip (`on_session_updated`)
    /// for a session update: resolves the bound plugin (if any), invokes it, and
    /// returns the negotiated capabilities JSON plus any plugin-returned
    /// metadata. When no plugin is bound, returns the request capabilities
    /// verbatim and no metadata.
    async fn negotiate_capabilities(
        &self,
        identity: &Identity,
        row: &Session,
        session_id: Uuid,
        caps: &[CapabilityValue],
    ) -> Result<(JsonValue, Option<JsonValue>)> {
        let session_type_id = row.session_type_id;
        let plugin_instance_id = match session_type_id {
            Some(st_id) => self
                .session_types
                .find_by_id(st_id)
                .await?
                .and_then(|st| st.plugin_instance_id),
            None => None,
        };

        let mut new_caps_json = capability_values_to_json(caps);
        let mut plugin_metadata: Option<JsonValue> = None;

        if let Some(ref plugin_instance_id) = plugin_instance_id {
            let plugin = self.plugins.resolve(plugin_instance_id)?;
            let plugin_config = match session_type_id {
                Some(st_id) => self.plugins.load_config(plugin_instance_id, st_id).await?,
                None => None,
            };
            let cancel = CancellationToken::new();
            let call_ctx = PluginCallContext {
                request_id: Uuid::new_v4(),
                tenant_id: TenantId::new(identity.tenant_id.as_str()),
                user_id: UserId::new(identity.user_id.as_str()),
                plugin_instance_id: plugin_instance_id.clone(),
                session_type_id: session_type_id.unwrap_or_else(Uuid::nil),
                plugin_config,
                enabled_capabilities: Some(caps.to_vec()),
                deadline: Some(Instant::now() + self.plugin_timeout),
                cancel: cancel.clone(),
            };
            let session_ctx = SessionPluginCtx {
                session_type_id: session_type_id.unwrap_or_else(Uuid::nil),
                session_id: Some(session_id),
                call_ctx,
            };
            // Plugin failure on update → 502 (mapped via the standard
            // PluginError → ChatEngineError conversion).
            let returned = self
                .invoke_with_deadline(plugin.on_session_updated(session_ctx), &cancel)
                .await?;
            new_caps_json = capabilities_to_json(returned.capabilities);
            plugin_metadata = returned.metadata;
        }

        Ok((new_caps_json, plugin_metadata))
    }

    pub async fn archive_session(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
    ) -> Result<Session> {
        let identity = identity_from_ctx(ctx)?;
        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::UPDATE)
            .await?;
        let from = row.lifecycle_state;
        ensure_can_transition(from, LifecycleState::Archived)?;

        let updated = self
            .sessions
            .update_lifecycle_state_scoped(&scope, session_id, LifecycleState::Archived)
            .await?;
        self.webhooks
            .emit(WebhookEvent::SessionArchived {
                session_id,
                tenant_id: identity.tenant_id.clone(),
                user_id: identity.user_id.clone(),
            })
            .await
            .unwrap_or_else(|err| warn!(error = %err, "webhook emit failed for session.archived"));
        Ok(redact_session(updated))
    }

    pub async fn restore_session(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
    ) -> Result<Session> {
        let identity = identity_from_ctx(ctx)?;
        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::UPDATE)
            .await?;
        let from = row.lifecycle_state;
        ensure_can_transition(from, LifecycleState::Active)?;

        // Refuse to restore once the hard-delete window has passed (per
        // ADR-0021): the row is technically still readable but the spec
        // requires a clear failure rather than silently re-arming a
        // session that's about to be reaped. The soft-delete bookkeeping
        // column is persistence-internal, so it comes from a targeted repo
        // query rather than the domain `Session`.
        let scheduled_hard_delete_at = self
            .sessions
            .scheduled_hard_delete_at_scoped(&scope, session_id)
            .await?;
        if let Some(scheduled) = scheduled_hard_delete_at
            && scheduled < OffsetDateTime::now_utc()
        {
            return Err(ChatEngineError::conflict(
                "soft-delete grace period elapsed; session can no longer be restored",
            ));
        }

        let updated = self
            .sessions
            .update_lifecycle_state_scoped(&scope, session_id, LifecycleState::Active)
            .await?;
        self.webhooks
            .emit(WebhookEvent::SessionRestored {
                session_id,
                tenant_id: identity.tenant_id.clone(),
                user_id: identity.user_id.clone(),
            })
            .await
            .unwrap_or_else(|err| warn!(error = %err, "webhook emit failed for session.restored"));
        Ok(redact_session(updated))
    }

    pub async fn delete_session(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        hard: bool,
    ) -> Result<SessionDeleteOutcome> {
        let identity = identity_from_ctx(ctx)?;
        let (row, scope) = self
            .authorize_session_op(ctx, session_id, actions::DELETE)
            .await?;
        let from = row.lifecycle_state;
        let target = if hard {
            LifecycleState::HardDeleted
        } else {
            LifecycleState::SoftDeleted
        };
        ensure_can_transition(from, target)?;

        if hard {
            // `hard_delete` performs the message/reaction cascade and stays on
            // the trusted (tenant, user) scope until Phase 7 (bypass registry);
            // access is already gated by the PDP DELETE decision above.
            let removed = self
                .sessions
                .hard_delete(&identity.tenant_id, &identity.user_id, session_id)
                .await?;
            if !removed {
                return Err(ChatEngineError::not_found("session", session_id));
            }
            self.webhooks
                .emit(WebhookEvent::SessionHardDeleted {
                    session_id,
                    tenant_id: identity.tenant_id.clone(),
                    user_id: identity.user_id.clone(),
                })
                .await
                .unwrap_or_else(|err| {
                    warn!(error = %err, "webhook emit failed for session.hard_deleted");
                });
            Ok(SessionDeleteOutcome::Hard)
        } else {
            let retention_days = DEFAULT_SOFT_DELETE_RETENTION_DAYS;
            let updated = self
                .sessions
                .soft_delete_scoped(&scope, session_id, retention_days)
                .await?;
            // Plugin notification for soft-delete is best-effort — the
            // current SDK trait (`ChatEngineBackendPlugin`) does NOT expose
            // an `on_session_deleted` hook. Phase 4 wires the webhook
            // emission; the plugin-trait extension lives in a future
            // version of the SDK (out of scope here).
            self.webhooks
                .emit(WebhookEvent::SessionSoftDeleted {
                    session_id,
                    tenant_id: identity.tenant_id.clone(),
                    user_id: identity.user_id.clone(),
                })
                .await
                .unwrap_or_else(|err| {
                    warn!(error = %err, "webhook emit failed for session.soft_deleted");
                });
            Ok(SessionDeleteOutcome::Soft {
                session: redact_session(updated),
            })
        }
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// Trusted prefetch of a session by id, followed by the PDP decision for
    /// `action`. Returns the prefetched row (source of the owner pair passed to
    /// the PDP as ABAC input) plus the compiled [`AccessScope`]. A denied,
    /// unreachable, or uncompilable PDP decision fails closed to `Forbidden`
    /// via the `?`-converted `EnforcerError` (DESIGN §3.5.5).
    ///
    /// `require_constraints(false)` lets the PDP answer allow/deny without
    /// mandating row-level constraints: an unconstrained scope means "allowed,
    /// no filter" (caller may use the prefetch); a constrained scope drives a
    /// scoped re-read / mutation whose 0-row outcome maps to `NotFound`.
    // @cpt-cf-chat-engine-seq-authz-point-op
    async fn authorize_session_op(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        action: &str,
    ) -> Result<(Session, AccessScope)> {
        // AUTHZ-BYPASS: system-internal owner-pair prefetch preceding the PDP
        // decision that authorizes this op; scoped by session_id.
        // @cpt-cf-chat-engine-design-authz-bypass-registry
        let prefetch = self
            .sessions
            .find_by_id_scoped(&bypass::system_read_scope(), session_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("session", session_id))?;

        // The PDP is asked for a decision, but ownership is a gear invariant:
        // no shipped policy plugin constrains `owner_id`, so a tenant-only
        // scope would admit a same-tenant stranger. Enforce the owner pair on
        // the trusted prefetch first — the PDP may only narrow from here.
        // @cpt-cf-chat-engine-nfr-authentication
        owner_guard::ensure_session_owner(ctx, &prefetch)?;

        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        let scope = self
            .enforcer
            .access_scope_with(
                ctx,
                &resource_types::SESSION,
                action,
                Some(session_id),
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, prefetch.tenant_id.as_str())
                    .resource_property(pep_properties::OWNER_ID, prefetch.user_id.as_str())
                    .require_constraints(false),
            )
            .await?;

        // Apply the PDP decision to the ROW, not just to downstream writes: a
        // constrained decision MUST be validated by re-reading under the
        // compiled scope, so a cross-tenant / out-of-scope target resolves to
        // 0 rows → NotFound (anti-enumeration, ADR-0021) instead of leaking the
        // unrestricted prefetch. An unconstrained decision (PDP allowed with no
        // row filter) safely uses the prefetch.
        let row = if scope.is_unconstrained() {
            prefetch
        } else {
            self.sessions
                .find_by_id_scoped(&scope, session_id)
                .await?
                .ok_or_else(|| ChatEngineError::not_found("session", session_id))?
        };
        Ok((row, scope))
    }

    /// Run a plugin call future against the cancellation token + deadline
    /// agreed on the [`PluginCallContext`]. On deadline elapse we cancel the
    /// token (so the plugin observes the signal) and return
    /// `BackendUnavailable` mapped via the standard `PluginError::timeout`
    /// path.
    async fn invoke_with_deadline<F, T>(&self, fut: F, cancel: &CancellationToken) -> Result<T>
    where
        F: std::future::Future<
                Output = std::result::Result<T, chat_engine_sdk::error::PluginError>,
            >,
    {
        match timeout(self.plugin_timeout, fut).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e.into()),
            Err(_elapsed) => {
                cancel.cancel();
                Err(chat_engine_sdk::error::PluginError::timeout().into())
            }
        }
    }
}

// ---------------- Free helpers used by tests + handlers ----------------

/// Build a service [`Identity`] from the JWT-derived [`SecurityContext`] for
/// plugin-call construction only. The authorization boundary is the enforcer /
/// `SecurityContext`; this helper just carries the opaque tenant/user strings
/// plugins expect. Anonymous contexts (nil subject/tenant) are rejected with
/// `Forbidden`.
pub(crate) fn identity_from_ctx(ctx: &SecurityContext) -> Result<Identity> {
    let tenant = ctx.subject_tenant_id();
    let user = ctx.subject_id();
    if tenant.is_nil() || user.is_nil() {
        return Err(ChatEngineError::forbidden(
            "authenticated identity required (tenant_id and user_id must be present in the bearer token)",
        ));
    }
    Identity::new(tenant.to_string(), user.to_string(), None)
}

/// Reject metadata payloads that try to write a reserved key. Centralised so
/// every handler/service entry point applies the same rule (per ADR-0017).
pub fn reject_reserved_metadata(metadata: Option<&JsonValue>) -> Result<()> {
    let Some(JsonValue::Object(map)) = metadata else {
        return Ok(());
    };
    for key in RESERVED_METADATA_KEYS {
        if map.contains_key(*key) {
            return Err(ChatEngineError::bad_request(format!(
                "metadata key '{key}' is reserved and cannot be set by clients"
            )));
        }
    }
    Ok(())
}

fn capabilities_to_json(caps: Vec<Capability>) -> JsonValue {
    serde_json::to_value(caps).unwrap_or(JsonValue::Array(Vec::new()))
}

fn capability_values_to_json(caps: &[CapabilityValue]) -> JsonValue {
    serde_json::to_value(caps).unwrap_or(JsonValue::Array(Vec::new()))
}

/// Merge plugin-supplied `overlay` metadata into the session's existing `base`
/// metadata (FR session-hook metadata). Object-level merge: overlay keys
/// override same-name base keys. Engine-reserved keys
/// (`memory_strategy` / `retention_policy` / `share_expires_at`) are stripped
/// from the overlay so a plugin can't clobber engine-managed session state.
/// A non-object overlay is ignored when the base is an object (to avoid
/// dropping client metadata); otherwise the overlay wins.
pub(crate) fn merge_plugin_metadata(base: Option<JsonValue>, overlay: JsonValue) -> JsonValue {
    let overlay = match overlay {
        JsonValue::Object(mut map) => {
            map.retain(|k, _| !RESERVED_METADATA_KEYS.contains(&k.as_str()));
            JsonValue::Object(map)
        }
        other => other,
    };
    match base {
        Some(JsonValue::Object(mut base_map)) => {
            if let JsonValue::Object(over_map) = overlay {
                for (k, v) in over_map {
                    base_map.insert(k, v);
                }
            }
            JsonValue::Object(base_map)
        }
        _ => overlay,
    }
}

/// Strip reserved metadata keys and clear the `share_token` (which is a
/// bearer secret) before returning a session to the caller. Phase 14 DTO
/// mapping must call this helper before serialization.
#[must_use]
pub fn redact_session(mut s: Session) -> Session {
    s.metadata = public_metadata(&s);
    // share_token is owned by Phase 10 — Phase 4 must not leak it.
    s.share_token = None;
    s
}

#[cfg(test)]
#[path = "session_service_tests.rs"]
mod session_service_tests;
