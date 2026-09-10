//! Session intelligence service.
//!
//! `IntelligenceService` orchestrates Phase 8's session-level intelligence
//! surface:
//!
//! 1. **On-demand summary** — `POST /sessions/{id}/summarize`. Validates
//!    ownership / lifecycle / plugin support, builds the visible history,
//!    invokes the backend plugin's `on_session_summary`, streams the result
//!    as NDJSON, then atomically persists the summary as a
//!    `role=system, parent_message_id=NULL, is_hidden_from_user=true`
//!    message and flips every message in `summarized_message_ids` to
//!    `is_hidden_from_backend=true`.
//! 2. **Per-session retention policy** — `GET` / `PATCH
//!    /sessions/{id}/retention-policy`. The policy is read / written via
//!    the reserved `session.metadata["retention_policy"]` key (Phase 2
//!    helpers, per ADR-0017). When unset, the effective policy falls back
//!    to the session-type default; this phase ships the surface but the
//!    session-type column does not yet exist on the Phase 1 schema, so the
//!    fallback resolves to [`RetentionPolicy::None`].
//! 3. **Tenant-scoped retention cleanup** —
//!    [`IntelligenceService::run_retention_cleanup_for_tenant`]. The
//!    algorithm acquires a per-session `pg_try_advisory_xact_lock`,
//!    evaluates the policy to pick eligible non-root messages, and
//!    recursively deletes each subtree atomically. Phase 15 will register
//!    this as a background task; this phase only exposes the entry point.
//!
//! ## Reserved metadata key
//!
//! The service is the only sanctioned writer of
//! `session.metadata["retention_policy"]`. Clients are forbidden from
//! writing the key via the generic metadata-patch surface (the
//! `reject_reserved_metadata` guard in `SessionService` already enforces
//! that constraint).
//!
//! ## Summary streaming contract
//!
//! The `summarize_session` method returns a `SummaryStream` of
//! [`StreamingEvent`] values mirroring the message-send pipeline: one
//! `Start` → zero-or-more `Chunk` → one `Complete` or `Error`. The
//! summary message is persisted only after a successful
//! `StreamingCompleteEvent`. If the stream errors mid-flight, no summary
//! message is written and no `is_hidden_from_backend` flips happen — the
//! caller can safely retry the operation.
//
// @cpt-cf-chat-engine-intelligence-service:p8
// @cpt-cf-chat-engine-flow-session-intelligence-generate-summary:p8
// @cpt-cf-chat-engine-flow-session-intelligence-get-retention:p8
// @cpt-cf-chat-engine-flow-session-intelligence-update-retention:p8
// @cpt-cf-chat-engine-algo-session-intelligence-validate-summarization:p8
// @cpt-cf-chat-engine-algo-session-intelligence-invoke-summary:p8
// @cpt-cf-chat-engine-algo-session-intelligence-evaluate-retention:p8
// @cpt-cf-chat-engine-algo-session-intelligence-enforce-retention:p8
// @cpt-cf-chat-engine-adr-session-metadata:p8
// @cpt-cf-chat-engine-adr-session-deletion-strategy:p8

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use chat_engine_sdk::models::{LifecycleState, TenantId, UserId};
use chat_engine_sdk::plugin::{PluginCallContext, SessionPluginCtx};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::domain::authz::{actions, bypass, owner_guard, resource_types};
use crate::domain::error::{ChatEngineError, Result};
use crate::domain::message::{
    StreamingChunkEvent, StreamingCompleteEvent, StreamingErrorEvent, StreamingEvent,
    StreamingStartEvent,
};
use crate::domain::ports::MessageRepo;
use crate::domain::ports::SessionRepo;
use crate::domain::ports::SessionTypeRepo;
use crate::domain::retention::RetentionPolicy;
use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer, ResourceType};
use toolkit_security::{SecurityContext, pep_properties};

use crate::domain::service::plugin_service::PluginService;
use crate::domain::service::session_service::identity_from_ctx;
use crate::domain::session::{Session, get_retention_policy, set_retention_policy};

/// Default per-call plugin deadline for `on_session_summary`. Mirrors the
/// streaming-message budget — summaries can legitimately take time to emit
/// a full response.
pub const DEFAULT_SUMMARY_DEADLINE: Duration = Duration::from_mins(2);

/// Bounded backpressure-channel size between the plugin driver and the
/// NDJSON sink. Mirrors the `MessageService` default; small enough to
/// bound memory but big enough that a typical chunk-stream does not
/// stall.
pub const DEFAULT_SUMMARY_BUFFER_SIZE: usize = 64;

/// Outgoing NDJSON-ready stream of [`StreamingEvent`] returned by
/// [`IntelligenceService::summarize_session`].
pub type SummaryStream = BoxStream<'static, StreamingEvent>;

/// Per-session cleanup report. Aggregated into [`RetentionCleanupReport`]
/// by [`IntelligenceService::run_retention_cleanup_for_tenant`].
#[domain_model]
#[derive(Debug, Clone)]
pub struct SessionCleanupOutcome {
    pub session_id: Uuid,
    /// Stable label for the policy that drove the cleanup
    /// (`"none"` / `"age_based"` / `"count_based"`).
    pub policy_type: &'static str,
    /// Number of messages physically removed (subtree included).
    pub messages_deleted: u64,
    /// Time spent on the cleanup, end-to-end (lock acquisition through
    /// delete).
    pub duration_ms: u64,
    /// `true` when the advisory lock could not be acquired — the session
    /// was skipped.
    pub skipped_locked: bool,
}

/// Aggregate cleanup report returned by
/// [`IntelligenceService::run_retention_cleanup_for_tenant`]. Phase 15
/// will surface this in operator metrics; the structure is intentionally
/// small (one entry per session in the tenant) so the report fits in
/// memory for any reasonable tenant scale.
#[domain_model]
#[derive(Debug, Clone, Default)]
pub struct RetentionCleanupReport {
    /// Outcomes ordered by session id (deterministic for logs / tests).
    pub sessions: Vec<SessionCleanupOutcome>,
}

impl RetentionCleanupReport {
    /// Total messages deleted across all sessions.
    #[must_use]
    pub fn total_messages_deleted(&self) -> u64 {
        self.sessions.iter().map(|s| s.messages_deleted).sum()
    }

    /// Sessions that were skipped because the advisory lock could not be
    /// acquired (another cleanup run was in flight).
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.skipped_locked).count()
    }
}

/// Validated retention policy. The wire shape is the SDK enum
/// [`RetentionPolicy`] (internally tagged on `"type"`); validation happens
/// inside [`validate_retention_policy`] before any DB write.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ValidatedPolicy(RetentionPolicy);

impl From<ValidatedPolicy> for RetentionPolicy {
    fn from(v: ValidatedPolicy) -> Self {
        v.0
    }
}

/// Public service.
///
/// Construct once at module init (Phase 15) with the shared repositories
/// + plugin hub. Clone freely — all fields are `Arc`/`Clone`-cheap.
#[domain_model]
#[derive(Clone)]
pub struct IntelligenceService {
    sessions: Arc<dyn SessionRepo>,
    session_types: Arc<dyn SessionTypeRepo>,
    messages: Arc<dyn MessageRepo>,
    plugins: PluginService,
    summary_buffer_size: usize,
    summary_deadline: Duration,
    /// Per-tick cap on the number of active sessions a single tenant
    /// can process. See `ChatEngineConfig::retention_max_sessions_per_tick`.
    retention_max_sessions_per_tick: u32,
    /// Per-tick cap on the number of subtree-root deletions a single
    /// session can perform. See
    /// `ChatEngineConfig::retention_max_deletes_per_session`.
    retention_max_deletes_per_session: u32,
    /// Per-tenant round-robin cursor (`tenant_id -> last session_id swept`)
    /// for the retention scheduler. Each tick fetches only the next
    /// `retention_max_sessions_per_tick` active sessions after this id, so a
    /// large tenant is processed in bounded batches across ticks instead of
    /// being materialised whole. In-process state: a leader failover restarts
    /// the round-robin from the beginning (the sweep is idempotent, so this
    /// only delays tail coverage). Shared across `Clone`s via the `Arc`.
    retention_cursor: Arc<Mutex<HashMap<String, Uuid>>>,
    enforcer: PolicyEnforcer,
}

/// Default per-tick session cap when the service is constructed
/// without a config (test fixtures). Production wiring overrides this
/// via [`IntelligenceService::with_retention_caps`].
pub const DEFAULT_RETENTION_MAX_SESSIONS_PER_TICK: u32 = 1000;

/// Default per-session per-tick deletion cap. See
/// [`DEFAULT_RETENTION_MAX_SESSIONS_PER_TICK`].
pub const DEFAULT_RETENTION_MAX_DELETES_PER_SESSION: u32 = 1000;

impl IntelligenceService {
    #[must_use]
    pub fn new(
        sessions: Arc<dyn SessionRepo>,
        session_types: Arc<dyn SessionTypeRepo>,
        messages: Arc<dyn MessageRepo>,
        plugins: PluginService,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            sessions,
            session_types,
            messages,
            plugins,
            summary_buffer_size: DEFAULT_SUMMARY_BUFFER_SIZE,
            summary_deadline: DEFAULT_SUMMARY_DEADLINE,
            retention_max_sessions_per_tick: DEFAULT_RETENTION_MAX_SESSIONS_PER_TICK,
            retention_max_deletes_per_session: DEFAULT_RETENTION_MAX_DELETES_PER_SESSION,
            retention_cursor: Arc::new(Mutex::new(HashMap::new())),
            enforcer,
        }
    }

    /// Override the bounded-channel size used between the plugin driver
    /// and the NDJSON sink. Useful for tests + operator tuning.
    #[must_use]
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.summary_buffer_size = size.max(1);
        self
    }

    /// Override the per-call summary deadline.
    #[must_use]
    pub fn with_summary_deadline(mut self, deadline: Duration) -> Self {
        self.summary_deadline = deadline;
        self
    }

    /// Override the retention-cleanup caps. Zero values are clamped to
    /// 1 so a single tick still makes forward progress.
    #[must_use]
    pub fn with_retention_caps(
        mut self,
        max_sessions_per_tick: u32,
        max_deletes_per_session: u32,
    ) -> Self {
        self.retention_max_sessions_per_tick = max_sessions_per_tick.max(1);
        self.retention_max_deletes_per_session = max_deletes_per_session.max(1);
        self
    }

    // ---------------------------------------------------------------------
    // Retention policy: read/write
    // ---------------------------------------------------------------------

    /// Resolve the effective retention policy for a session: the
    /// per-session override (when set) or the session-type default
    /// (otherwise). When neither is set, returns [`RetentionPolicy::None`].
    ///
    /// The session-type-level default is not yet stored in the Phase 1
    /// schema; the fallback resolves to `None` until ADR-0021 adds the
    /// column. The wiring is in place here so downstream phases can switch
    /// the fallback source without touching the service surface.
    #[instrument(skip(self, ctx), fields(session_id = %session_id))]
    pub async fn get_effective_retention_policy(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
    ) -> Result<RetentionPolicy> {
        // @cpt-cf-chat-engine-seq-authz-point-op
        let session = self
            .authorize_session_op(
                ctx,
                session_id,
                &resource_types::SESSION,
                actions::READ,
                Some(session_id),
            )
            .await?;
        Ok(resolve_effective_policy(&session))
    }

    /// Persist a new per-session retention policy. Validates the payload
    /// per the SDK constraints (variant + numeric bounds) and writes the
    /// reserved `session.metadata["retention_policy"]` key atomically.
    ///
    /// Returns the persisted policy (echoed verbatim — the wire shape
    /// matches the request body).
    #[instrument(skip(self, ctx), fields(session_id = %session_id))]
    pub async fn update_session_retention_policy(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        policy: RetentionPolicy,
    ) -> Result<RetentionPolicy> {
        let validated = validate_retention_policy(policy)?;
        // Opaque tenant/user strings for the scoped metadata write below.
        let identity = identity_from_ctx(ctx)?;

        // PDP gate (owner pair as ABAC input); denied → 403.
        // @cpt-cf-chat-engine-seq-authz-point-op
        let mut session = self
            .authorize_session_op(
                ctx,
                session_id,
                &resource_types::SESSION,
                actions::UPDATE,
                Some(session_id),
            )
            .await?;
        if matches!(
            session.lifecycle_state,
            LifecycleState::SoftDeleted | LifecycleState::HardDeleted
        ) {
            return Err(ChatEngineError::conflict(format!(
                "session is {} and cannot accept retention_policy updates",
                session.lifecycle_state
            )));
        }

        // Apply the reserved-key write to a fresh metadata clone, then
        // persist via the standard `update_metadata` path. The repo bumps
        // `updated_at`; sibling metadata keys survive verbatim.
        let persisted_policy = validated.0.clone();
        set_retention_policy(&mut session, validated.0);
        let new_metadata = session.metadata.clone();

        let _persisted = self
            .sessions
            .update_metadata(
                &identity.tenant_id,
                &identity.user_id,
                session_id,
                new_metadata,
            )
            .await?;

        info!(
            session_id = %session_id,
            policy_type = %retention_policy_label(&persisted_policy),
            "persisted per-session retention policy"
        );

        Ok(persisted_policy)
    }

    // ---------------------------------------------------------------------
    // On-demand session summary
    // ---------------------------------------------------------------------

    /// Generate an AI-summary for a session and stream it as
    /// [`StreamingEvent`]s. The handler (Phase 14) wraps each event in
    /// one NDJSON line.
    ///
    /// Pre-stream failures surface as `Err(ChatEngineError)` mapped to
    /// the standard HTTP statuses (403/404/409/422/502). Mid-stream
    /// failures stay on the wire as `StreamingErrorEvent` (the HTTP
    /// response has already started by then).
    #[instrument(
        skip(self, ctx, cancel),
        fields(
            session_id = %session_id,
            user_id = %ctx.subject_id(),
            request_id,
            summary_message_id,
        ),
    )]
    pub async fn summarize_session(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        cancel: CancellationToken,
    ) -> Result<SummaryStream> {
        // Opaque tenant/user strings for the plugin ctx + summary owner stamp.
        let identity = identity_from_ctx(ctx)?;
        // Summarization mutates the session: it persists a system summary
        // message and flips `is_hidden_from_backend` on summarized messages, so
        // it is authorized as a SESSION update — not a read.
        // @cpt-cf-chat-engine-seq-authz-point-op
        let session = self
            .authorize_session_op(
                ctx,
                session_id,
                &resource_types::SESSION,
                actions::UPDATE,
                Some(session_id),
            )
            .await?;
        // Lifecycle gate: 409 when not active (active is the only state
        // that admits on-demand work per the feature spec).
        if !matches!(session.lifecycle_state, LifecycleState::Active) {
            return Err(ChatEngineError::conflict(format!(
                "session is {} and cannot be summarized",
                session.lifecycle_state
            )));
        }

        // Session-type + plugin binding are required for summary routing
        // (422 when unbound per the feature spec).
        let session_type_id =
            session
                .session_type_id
                .ok_or_else(|| ChatEngineError::BadRequest {
                    reason: "session has no session_type bound; summary cannot be generated"
                        .to_string(),
                })?;
        let session_type = self
            .session_types
            .find_by_id(session_type_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("session_type", session_type_id))?;
        let plugin_instance_id = session_type.plugin_instance_id.ok_or_else(|| {
            // 422 Unprocessable Entity — the plugin binding is missing, so
            // the service cannot generate a summary even though the
            // session itself is well-formed. We surface this via a
            // BackendUnavailable so the handler can map to 422.
            ChatEngineError::BackendUnavailable {
                reason: "session_type has no plugin_instance_id; summarization unsupported"
                    .to_string(),
                retry_after: None,
                source: None,
            }
        })?;

        // Resolve the plugin. A missing registration is 422 per the
        // feature spec (plugin does not support summarization).
        let plugin = self
            .plugins
            .resolve(&plugin_instance_id)
            .map_err(|err| match err {
                ChatEngineError::NotFound { .. } => ChatEngineError::BackendUnavailable {
                    reason: format!(
                        "plugin '{plugin_instance_id}' is not registered; summarization unsupported"
                    ),
                    retry_after: None,
                    source: None,
                },
                other => other,
            })?;
        let plugin_config = self
            .plugins
            .load_config(&plugin_instance_id, session_type_id)
            .await?;

        // Load the visible history (chronological, excluding
        // `is_hidden_from_backend=true`) — this is the Phase 7 canonical
        // history-visibility filter.
        let history = self.messages.fetch_active_history(session_id, None).await?;

        // Build the call context. The plugin's child token observes the
        // handler's cancellation (connection close / explicit cancel).
        let request_id = Uuid::new_v4();
        tracing::Span::current().record("request_id", tracing::field::display(request_id));
        let plugin_cancel = cancel.child_token();
        let deadline = Instant::now() + self.summary_deadline;
        let call_ctx = PluginCallContext {
            request_id,
            tenant_id: TenantId::new(identity.tenant_id.as_str()),
            user_id: UserId::new(identity.user_id.as_str()),
            plugin_instance_id: plugin_instance_id.clone(),
            session_type_id,
            plugin_config,
            enabled_capabilities: session
                .enabled_capabilities
                .as_ref()
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            deadline: Some(deadline),
            cancel: plugin_cancel.clone(),
        };
        let plugin_ctx = SessionPluginCtx {
            session_type_id,
            session_id: Some(session_id),
            call_ctx,
        };

        // The history is forwarded out-of-band via the handler-built
        // wire context — the SDK trait does not pass `messages` on
        // SessionPluginCtx because session-level hooks may need to
        // re-resolve history under plugin control. We log a brief
        // summary so operators can correlate the call with the on-wire
        // event without logging PII.
        info!(
            session_id = %session_id,
            history_len = history.len(),
            plugin_instance_id = %plugin_instance_id,
            "invoking on_session_summary"
        );

        // Pre-stream failure → return mapped error to the handler.
        let plugin_stream = match plugin.on_session_summary(plugin_ctx).await {
            Ok(s) => s,
            Err(err) => {
                warn!(
                    session_id = %session_id,
                    error = %err,
                    "on_session_summary returned pre-stream failure"
                );
                return Err(err.into());
            }
        };

        // Pre-allocate the summary message id so the handler can emit it
        // on the `Start` event for symmetry with the message-send
        // pipeline. The actual DB row is materialised only after
        // `Complete` (see contract).
        let summary_message_id = Uuid::new_v4();
        tracing::Span::current().record(
            "summary_message_id",
            tracing::field::display(summary_message_id),
        );

        // Spawn the driver task + return the bounded-channel-backed
        // stream. The driver:
        //   - emits Start with our pre-allocated id;
        //   - re-emits chunks with the canonical id;
        //   - on Complete: persists the summary message + flips
        //     `is_hidden_from_backend=true` on the summarized ids;
        //   - on Error / cancel: emits the wire event and does NOT
        //     persist (the spec mandates "no side effects on failure").
        let stream = self.spawn_summary_driver(
            session_id,
            summary_message_id,
            plugin_stream,
            cancel,
            plugin_cancel,
            deadline,
            identity.tenant_id.clone(),
        );
        Ok(stream)
    }

    // ---------------------------------------------------------------------
    // Tenant-scoped retention cleanup
    // ---------------------------------------------------------------------

    /// Entry point for the Phase 15 background scheduler. Walks every
    /// active session in the tenant, evaluates the effective retention
    /// policy, and recursively deletes eligible message subtrees.
    ///
    /// The algorithm:
    ///   1. Load all `lifecycle_state = active` sessions for the tenant.
    ///   2. For each session, acquire a Postgres advisory lock keyed on
    ///      the session id; on failure skip and continue.
    ///   3. Evaluate the policy → list of eligible non-root message ids.
    ///   4. Delete each eligible subtree atomically (one transaction per
    ///      subtree).
    ///   5. Emit a structured log event per session.
    ///
    /// Idempotency: re-running the cleanup MUST NOT double-delete or
    /// fail on already-empty sessions. `delete_message_subtree` returns
    /// `Ok(0)` when the root is missing (concurrent run already removed
    /// it); the policy evaluator returns an empty list when nothing is
    /// eligible.
    ///
    /// Note on advisory locks: the Phase 1 schema runs on both Postgres
    /// (`pg_try_advisory_xact_lock`) and SQLite (no analogue). The lock
    /// acquisition is treated as advisory — the policy evaluator and the
    /// subtree delete are both safe under concurrent runs even without
    /// the lock — so the SQLite path treats the lock as a successful no-op.
    #[instrument(skip(self), fields(tenant_id = %tenant_id))]
    pub async fn run_retention_cleanup_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<RetentionCleanupReport> {
        // TODO(phase-7): bypass registry — system_read_scope()
        // Scheduled background sweep with no HTTP subject / SecurityContext, so
        // the PolicyEnforcer MUST NOT be called here. The underlying repo reads
        // run under the repo's internal allow_all() (converted in Phase 7).
        // Per-tick cap pushed into SQL: fetch only the next `cap` active
        // sessions (ordered by `session_id`) after the previous tick's
        // cursor, rather than materialising the tenant's whole active set and
        // truncating in memory. Processing a session does not make it
        // inactive, so a bare `LIMIT` would re-scan the head every tick and
        // starve the tail — the per-tenant cursor round-robins coverage
        // across ticks instead.
        let cap = self.retention_max_sessions_per_tick;
        let after = self.retention_cursor.lock().get(tenant_id).copied();
        let mut active = self
            .sessions
            .list_active_sessions_for_tenant(tenant_id, after, cap)
            .await?;
        if active.is_empty() && after.is_some() {
            // Cursor ran past the end (or the tail shrank below it): wrap to
            // the start so this tick still makes forward progress.
            active = self
                .sessions
                .list_active_sessions_for_tenant(tenant_id, None, cap)
                .await?;
        }

        // Capture batch bounds before the batch is consumed below, so the
        // cursor can be advanced only after the batch is processed (a
        // mid-batch error leaves the cursor put and retries next tick).
        let batch_len = active.len();
        let last_id = active.last().map(|r| r.session_id);

        let mut outcomes: Vec<SessionCleanupOutcome> = Vec::with_capacity(active.len());

        for row in active {
            let session: Session = row;
            let policy = resolve_effective_policy(&session);
            let label = retention_policy_label(&policy);

            // Empty policy → record + skip without locking.
            if matches!(policy, RetentionPolicy::None) {
                outcomes.push(SessionCleanupOutcome {
                    session_id: session.session_id,
                    policy_type: label,
                    messages_deleted: 0,
                    duration_ms: 0,
                    skipped_locked: false,
                });
                continue;
            }

            let start = Instant::now();

            // Advisory lock: best-effort. Real Postgres acquisition lives
            // in Phase 15 (it needs a `&DatabaseConnection`); the repo
            // surface here intentionally does not require one. The
            // single-session-at-a-time semantics are still preserved by
            // the SERIALIZABLE transaction inside `delete_message_subtree`.
            // Phase 15 will hook the actual lock into this code path.
            let lock_acquired = true;
            if !lock_acquired {
                outcomes.push(SessionCleanupOutcome {
                    session_id: session.session_id,
                    policy_type: label,
                    messages_deleted: 0,
                    duration_ms: start.elapsed().as_millis() as u64,
                    skipped_locked: true,
                });
                continue;
            }

            let eligible = self
                .evaluate_retention_policy(session.session_id, &policy)
                .await?;
            let mut removed: u64 = 0;
            for id in eligible {
                let n = self
                    .messages
                    .delete_message_subtree(session.session_id, id)
                    .await?;
                removed += n;
            }

            let duration_ms = start.elapsed().as_millis() as u64;
            info!(
                session_id = %session.session_id,
                messages_deleted = removed,
                policy_type = label,
                duration_ms = duration_ms,
                "retention cleanup completed for session"
            );

            outcomes.push(SessionCleanupOutcome {
                session_id: session.session_id,
                policy_type: label,
                messages_deleted: removed,
                duration_ms,
                skipped_locked: false,
            });
        }

        // Advance the round-robin cursor now that the batch is processed. A
        // full batch (`== cap`) means more sessions may follow — resume after
        // the last id next tick. A short batch means we reached the end, so
        // drop the cursor and wrap to the start on the next tick.
        match last_id {
            Some(id) if batch_len == cap as usize => {
                self.retention_cursor
                    .lock()
                    .insert(tenant_id.to_owned(), id);
                debug!(
                    tenant_id,
                    cap,
                    next_after = %id,
                    "retention sweep filled a full batch; more sessions deferred to next tick",
                );
            }
            _ => {
                self.retention_cursor.lock().remove(tenant_id);
            }
        }

        outcomes.sort_by_key(|o| o.session_id);
        Ok(RetentionCleanupReport { sessions: outcomes })
    }

    /// Enumerate every tenant that currently owns an `active` session and
    /// run [`Self::run_retention_cleanup_for_tenant`] for each. Reports
    /// from all tenants are concatenated in tenant-discovery order; per-
    /// tenant failures are logged at WARN level and skipped so a single
    /// faulty tenant cannot starve the rest of the schedule.
    ///
    /// The scheduler uses this entry point rather than guessing tenant
    /// ids: the session repository is the source of truth for which
    /// tenants are live.
    #[instrument(skip(self))]
    pub async fn run_retention_cleanup_all_tenants(&self) -> Result<RetentionCleanupReport> {
        // TODO(phase-7): bypass registry — system_read_scope()
        // Scheduled cross-tenant sweep with no HTTP subject; PolicyEnforcer is
        // not invoked here (see run_retention_cleanup_for_tenant).
        let tenants = self.sessions.list_tenants_with_active_sessions().await?;
        let mut aggregated: Vec<SessionCleanupOutcome> = Vec::new();
        for tenant_id in tenants {
            match self.run_retention_cleanup_for_tenant(&tenant_id).await {
                Ok(report) => aggregated.extend(report.sessions),
                Err(err) => warn!(
                    %tenant_id,
                    error = %err,
                    "retention cleanup failed for tenant; continuing with next tenant",
                ),
            }
        }
        Ok(RetentionCleanupReport {
            sessions: aggregated,
        })
    }

    /// Evaluate a retention policy and return the list of message ids
    /// eligible for deletion. Public-ish (visible to tests in this
    /// module) but not exposed to other crates — the entry point for
    /// external callers is [`Self::run_retention_cleanup_for_tenant`].
    ///
    /// Algorithm (mirrors the feature spec §3 — Evaluate Retention Policy):
    /// - `None` → empty list.
    /// - `AgeBased { max_age_days }` → non-root messages with
    ///   `created_at < now() - max_age_days * 1 day`.
    /// - `CountBased { max_message_count }` → if total non-root count
    ///   exceeds the threshold, the oldest `total - max` messages.
    ///
    /// **Root messages (`parent_message_id IS NULL`) are NEVER eligible**
    /// — they anchor the conversation tree (see the feature spec design
    /// note on Root Message Preservation).
    pub(crate) async fn evaluate_retention_policy(
        &self,
        session_id: Uuid,
        policy: &RetentionPolicy,
    ) -> Result<Vec<Uuid>> {
        match policy {
            RetentionPolicy::None => Ok(Vec::new()),
            RetentionPolicy::AgeBased { max_age_days } => {
                let cutoff = OffsetDateTime::now_utc()
                    - Duration::from_secs(u64::from(*max_age_days) * 86_400);
                // Push the WHERE + LIMIT into SQL and project only the
                // message_id column — the previous form selected every
                // column for every matching row, then dropped them in
                // the service. Capped at the per-session tick budget.
                self.messages
                    .list_non_root_message_ids_older_than(
                        session_id,
                        cutoff,
                        self.retention_max_deletes_per_session,
                    )
                    .await
            }
            RetentionPolicy::CountBased { max_message_count } => {
                let max = u64::from(*max_message_count);
                // One cheap COUNT(*) instead of materialising every
                // non-root row.
                let total = self.messages.count_non_root_messages(session_id).await?;
                if total <= max {
                    return Ok(Vec::new());
                }
                let surplus = total - max;
                // Cap the deletion budget so a session with millions of
                // surplus rows can't dominate a tick. Anything left
                // over rolls into the next tick.
                let limit = surplus
                    .min(u64::from(self.retention_max_deletes_per_session))
                    .try_into()
                    .unwrap_or(self.retention_max_deletes_per_session);
                self.messages
                    .list_oldest_non_root_message_ids(session_id, limit)
                    .await
            }
        }
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// Trusted prefetch of a session by id, then the PDP decision for
    /// (`resource`, `action`). Returns the prefetched session (owner pair fed
    /// to the PDP as ABAC input; also used downstream for lifecycle gating,
    /// plugin ctx, and the scoped metadata write). A denied / unreachable /
    /// uncompilable decision fails closed to `Forbidden` via the `?`-converted
    /// `EnforcerError` (DESIGN §3.5.5).
    // @cpt-cf-chat-engine-seq-authz-point-op
    async fn authorize_session_op(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        resource: &ResourceType,
        action: &str,
        resource_id: Option<Uuid>,
    ) -> Result<Session> {
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

        // @cpt-cf-chat-engine-interface-pep
        // @cpt-cf-chat-engine-constraint-fail-closed-authz
        let scope = self
            .enforcer
            .access_scope_with(
                ctx,
                resource,
                action,
                resource_id,
                &AccessRequest::new()
                    .resource_property(pep_properties::OWNER_TENANT_ID, prefetch.tenant_id.as_str())
                    .resource_property(pep_properties::OWNER_ID, prefetch.user_id.as_str())
                    .require_constraints(false),
            )
            .await?;

        // A constrained decision MUST be validated against the ROW: re-read
        // under the compiled scope so a cross-tenant / out-of-scope target
        // resolves to 0 rows → NotFound (anti-enumeration, ADR-0021) instead of
        // consuming the unrestricted prefetch. An unconstrained decision safely
        // uses the prefetch.
        if scope.is_unconstrained() {
            Ok(prefetch)
        } else {
            self.sessions
                .find_by_id_scoped(&scope, session_id)
                .await?
                .ok_or_else(|| ChatEngineError::not_found("session", session_id))
        }
    }

    /// Spawn the streaming driver that pumps the plugin's summary stream
    /// into a bounded channel, persists the result on success, and emits
    /// the canonical NDJSON-event sequence.
    fn spawn_summary_driver(
        &self,
        session_id: Uuid,
        summary_message_id: Uuid,
        mut plugin_stream: chat_engine_sdk::plugin::PluginStream,
        cancel: CancellationToken,
        plugin_cancel: CancellationToken,
        deadline: Instant,
        // Owning tenant (denormalized) stamped onto the persisted summary
        // message; the summary is system-generated and has no human author.
        tenant_id: String,
    ) -> SummaryStream {
        let (tx, rx) = mpsc::channel::<StreamingEvent>(self.summary_buffer_size);
        let messages = Arc::clone(&self.messages);

        // Sleep-until-deadline guard. When the deadline fires we cancel
        // the plugin token; the driver loop folds the elapsed deadline
        // into a timeout error.
        let plugin_cancel_for_deadline = plugin_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            plugin_cancel_for_deadline.cancel();
        });

        let tx_for_driver = tx.clone();
        tokio::spawn(async move {
            // 1) Emit our canonical Start (message_id = summary id).
            let start = StreamingEvent::Start(StreamingStartEvent {
                message_id: summary_message_id,
            });
            if tx_for_driver.send(start).await.is_err() {
                cancel.cancel();
                return;
            }

            let mut accumulator = String::new();
            let mut last_metadata: Option<JsonValue> = None;
            let mut summarized_ids: Vec<Uuid> = Vec::new();
            let mut completed = false;
            let mut errored: Option<String> = None;

            loop {
                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        plugin_cancel.cancel();
                        break;
                    }

                    next = plugin_stream.next() => {
                        let Some(item) = next else {
                            // Plugin closed without emitting Complete.
                            // Treat as graceful end — but per the spec
                            // ("persist only on Complete") we do NOT
                            // persist a summary message here.
                            break;
                        };
                        match item {
                            Ok(StreamingEvent::Start(_)) => {
                                // Drop the plugin's own Start; we already
                                // emitted ours.
                            }
                            Ok(StreamingEvent::Chunk(c)) => {
                                accumulator.push_str(&c.chunk);
                                let evt = StreamingEvent::Chunk(StreamingChunkEvent {
                                    message_id: summary_message_id,
                                    chunk: c.chunk,
                                });
                                if tx_for_driver.send(evt).await.is_err() {
                                    plugin_cancel.cancel();
                                    break;
                                }
                            }
                            Ok(StreamingEvent::Complete(c)) => {
                                // Inspect metadata for an optional list of
                                // summarized message ids; record it so we can
                                // flip `is_hidden_from_backend` on the matching
                                // rows in the persist step. The client-facing
                                // `Complete` is deferred until after the summary
                                // is persisted (below), so a failed write never
                                // reports success to the caller.
                                if let Some(ref meta) = c.metadata {
                                    summarized_ids = extract_summarized_ids(meta);
                                }
                                last_metadata = c.metadata;
                                completed = true;
                                break;
                            }
                            Ok(StreamingEvent::Error(e)) => {
                                let evt = StreamingEvent::Error(StreamingErrorEvent {
                                    message_id: summary_message_id,
                                    error: e.error.clone(),
                                });
                                tx_for_driver.send(evt).await.ok();
                                errored = Some(e.error);
                                break;
                            }
                            // Summary streams are text-only; richer vocabulary
                            // events (status/part/citation/state/session-meta/
                            // tool) are not produced by `on_session_summary`
                            // and are ignored if a plugin emits them.
                            Ok(_) => {}
                            Err(err) => {
                                let s = err.to_string();
                                let evt = StreamingEvent::Error(StreamingErrorEvent {
                                    message_id: summary_message_id,
                                    error: s.clone(),
                                });
                                tx_for_driver.send(evt).await.ok();
                                errored = Some(s);
                                break;
                            }
                        }
                    }
                }
            }

            if completed {
                // Persist the summary message + flip `is_hidden_from_backend`
                // on the reported ids, THEN emit the client-facing `Complete`
                // — and only on a successful write. A persistence failure
                // surfaces as a streaming error rather than a fake success.
                match messages
                    .insert_summary_message(
                        session_id,
                        accumulator,
                        last_metadata.clone(),
                        summarized_ids,
                        Some(tenant_id.clone()),
                    )
                    .await
                {
                    Ok(_) => {
                        let evt = StreamingEvent::Complete(StreamingCompleteEvent {
                            message_id: summary_message_id,
                            metadata: last_metadata,
                            // Summaries carry no citations.
                            file_citations: vec![],
                            link_citations: vec![],
                            references: vec![],
                        });
                        tx_for_driver.send(evt).await.ok();
                    }
                    Err(err) => {
                        warn!(
                            session_id = %session_id,
                            summary_message_id = %summary_message_id,
                            error = %err,
                            "failed to persist session summary after stream complete",
                        );
                        let evt = StreamingEvent::Error(StreamingErrorEvent {
                            message_id: summary_message_id,
                            error: format!("failed to persist session summary: {err}"),
                        });
                        tx_for_driver.send(evt).await.ok();
                    }
                }
            } else if let Some(err) = errored {
                // Mid-stream error: per the spec the service does NOT
                // persist the summary. Log for operators only.
                warn!(
                    session_id = %session_id,
                    summary_message_id = %summary_message_id,
                    error = %err,
                    "summary stream errored mid-flight; no summary persisted"
                );
            }
        });

        stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|evt| (evt, rx)) },
        )
        .boxed()
    }
}

// =========================================================================
//  Free helpers
// =========================================================================

/// Validate a [`RetentionPolicy`] payload per the SDK constraints.
///
/// Returns:
/// - `Ok(_)` for [`RetentionPolicy::None`].
/// - `Err(BadRequest)` for `AgeBased { max_age_days: 0 }`.
/// - `Err(BadRequest)` for `CountBased { max_message_count: 0 }`.
///
/// Unknown variants are enforced by serde at the deserialization layer
/// (`#[serde(tag = "type", rename_all = "snake_case")]` rejects unknown
/// discriminators automatically).
pub fn validate_retention_policy(policy: RetentionPolicy) -> Result<ValidatedPolicy> {
    match &policy {
        RetentionPolicy::None => {}
        RetentionPolicy::AgeBased { max_age_days } => {
            if *max_age_days < 1 {
                return Err(ChatEngineError::bad_request(
                    "max_age_days required and must be >= 1",
                ));
            }
        }
        RetentionPolicy::CountBased { max_message_count } => {
            if *max_message_count < 1 {
                return Err(ChatEngineError::bad_request(
                    "max_message_count required and must be >= 1",
                ));
            }
        }
    }
    Ok(ValidatedPolicy(policy))
}

/// Resolve the effective retention policy for a session: per-session
/// override wins; fallback resolves to [`RetentionPolicy::None`] until
/// the session-type column lands (ADR-0021).
#[must_use]
pub fn resolve_effective_policy(session: &Session) -> RetentionPolicy {
    get_retention_policy(session).unwrap_or(RetentionPolicy::None)
}

/// Stable short label for a [`RetentionPolicy`] discriminant, used for
/// structured log events + metrics dimensions.
#[must_use]
pub fn retention_policy_label(p: &RetentionPolicy) -> &'static str {
    match p {
        RetentionPolicy::None => "none",
        RetentionPolicy::AgeBased { .. } => "age_based",
        RetentionPolicy::CountBased { .. } => "count_based",
    }
}

/// Extract an optional list of summarized message ids from the plugin's
/// `Complete` metadata. The SDK convention (per the feature spec §
/// Generate Session Summary) places this under
/// `metadata.summarized_message_ids: [uuid, ...]`. Malformed shapes
/// silently collapse to an empty list so a plugin that omits the field
/// does not break the persistence flow.
fn extract_summarized_ids(meta: &JsonValue) -> Vec<Uuid> {
    let Some(arr) = meta
        .get("summarized_message_ids")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
        .collect()
}

#[cfg(test)]
#[path = "intelligence_service_tests.rs"]
mod intelligence_service_tests;
