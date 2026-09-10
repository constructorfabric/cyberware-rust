//! Message processing & streaming service.
//!
//! `MessageService::send_message` is the core entrypoint for
//! `POST /messages/send`. It:
//!
//! 1. Validates ownership / lifecycle / payload via Phase-4 helpers and
//!    the local `validate_request` step.
//! 2. Inserts the user message + a pre-allocated assistant stub in a single
//!    SERIALIZABLE transaction (see
//!    [`crate::domain::ports::MessageRepo::insert_user_and_assistant_stub`]).
//! 3. Builds the active-path history and dispatches to the backend plugin
//!    via `ClientHub::get_scoped::<dyn ChatEngineBackendPlugin>`.
//! 4. Forwards the plugin's `PluginStream` through a bounded
//!    `tokio::sync::mpsc` channel (ADR-0010 backpressure) into a
//!    `BoxStream<StreamingEvent>` the handler maps to NDJSON.
//! 5. On `Complete` / `Error` / cancellation atomically finalises the
//!    assistant message (`is_complete=false` for cancel/error;
//!    `is_complete=true` for success).
//!
//! Cancellation is bridged via the [`tokio_util::sync::CancellationToken`]
//! the handler obtains from axum's connection-close future; the SDK
//! `PluginCallContext.cancel` is a child of that token.
//!
//! ## Extension hooks
//!
//! - [`MessageService::prepare_recreate_stub`] is the placeholder Phase 6
//!   (recreate) will fill in; the signature is stabilised here so the
//!   service surface area does not change later.
//! - [`MessageService::cancel_streaming`] exposes the cancellation
//!   primitive Phase 12 (`DELETE /streaming`) will call.
//
// @cpt-cf-chat-engine-message-service:p5
// @cpt-cf-chat-engine-adr-streaming-architecture:p5
// @cpt-cf-chat-engine-adr-streaming-cancellation:p5
// @cpt-cf-chat-engine-adr-backpressure-handling:p5

use std::sync::Arc;
use std::time::{Duration, Instant};

use chat_engine_sdk::error::PluginError;
use chat_engine_sdk::models::{
    CapabilityValue, LifecycleState, MessagePartInput, TenantId, UserId,
};
use chat_engine_sdk::plugin::{
    MessagePluginCtx, PluginCallContext, PluginStream, SessionPluginCtx,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use crate::domain::authz::{actions, bypass, owner_guard, resource_types};
use crate::domain::context::{
    is_context_overflow_error, read_memory_strategy, validate_memory_strategy,
    write_memory_strategy,
};
use crate::domain::error::{ChatEngineError, Result};
use crate::domain::memory_strategy::MemoryStrategy;
use crate::domain::message::{
    MAX_MESSAGE_METADATA_BYTES, Message, RESERVED_MESSAGE_METADATA_KEYS, StreamingChunkEvent,
    StreamingCompleteEvent, StreamingErrorEvent, StreamingEvent, StreamingStartEvent,
};
use crate::domain::ports::SessionRepo;
use crate::domain::ports::SessionTypeRepo;
use crate::domain::ports::StreamEventBuffer;
use crate::domain::ports::{
    FinalizeOutcome, InsertedPair, MessageRepo, NewUserMessage, PartCitations,
};
use crate::domain::service::plugin_service::PluginService;
use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};

use crate::domain::service::session_service::{Identity, identity_from_ctx, redact_session};
use crate::domain::service::webhook::{NoopWebhookEmitter, WebhookEmitter, WebhookEvent};
use crate::domain::session::Session;
use crate::domain::stream_delta::DeltaProjector;

/// Default maximum number of pending events in the bounded backpressure
/// channel between the plugin driver task and the NDJSON sink. Per
/// ADR-0010 the per-stream pending data MUST be ≤ 10MB; with the SDK's
/// recommended ~16KB chunk size that puts the safe element cap at ~640.
/// We start conservatively at 64 so a misbehaving plugin cannot blow the
/// memory budget; operators can tune via `config.streaming_buffer_size`.
pub const DEFAULT_STREAMING_BUFFER_SIZE: usize = 64;

/// Default plugin-call deadline for streaming. Longer than the lifecycle
/// hooks' 10s budget because plugins legitimately take time to emit a
/// full response — but bounded so a hung plugin still releases resources.
pub const DEFAULT_PLUGIN_DEADLINE: Duration = Duration::from_mins(2);

/// How long a resume-buffer event lives (FR-024). Bounds the reconnect window:
/// a client may resume within this window; afterwards it reconciles via
/// `GET /messages/{id}`. Long enough to cover a generous stream + a brief
/// reconnect gap, short enough that the buffer stays small.
pub const RESUME_BUFFER_TTL: time::Duration = time::Duration::minutes(10);

/// Tees a driver's outgoing `StreamingEvent`s to the bounded client channel
/// **and** (when a resume buffer is configured) into the buffer as projected,
/// `seq`-stamped wire events (FR-024). A single [`DeltaProjector`] instance is
/// kept so `seq` is monotonic across the whole stream. Buffer writes are
/// best-effort: a failed append is logged and never blocks the live stream.
///
/// True live-tail (FR-024): a dropped channel receiver is **not** fatal. The
/// driver keeps running to completion, buffering every event, so a
/// `Last-Event-ID` reconnect resumes seamlessly. Once the client is gone we
/// stop attempting channel sends that can no longer succeed.
///
/// The buffer projector here and the wire projector in
/// `api::rest::sse_delta_stream_response` are two independent
/// [`DeltaProjector`]s fed the *same* `StreamingEvent` sequence, so they assign
/// identical `seq`s. That invariant is what lets a client reconnect with the
/// `Last-Event-ID` it saw on the wire and have the buffer replay exactly the
/// undelivered tail.
#[domain_model]
struct Emitter {
    tx: mpsc::Sender<StreamingEvent>,
    projector: DeltaProjector,
    buffer: Option<Arc<dyn StreamEventBuffer>>,
    message_id: Uuid,
    expires_at: OffsetDateTime,
    /// Set once the channel receiver is gone (client disconnected). The driver
    /// runs on regardless; this just suppresses further doomed channel sends.
    client_gone: bool,
}

impl Emitter {
    /// Tee `event` into the resume buffer (best-effort) and, while the client
    /// is still connected, forward it to the live channel. A dropped receiver
    /// is non-fatal — the driver runs to completion either way (FR-024).
    async fn emit(&mut self, event: StreamingEvent) {
        if let Some(buffer) = &self.buffer {
            for wire in self.projector.project(event.clone()) {
                let value = serde_json::to_value(&wire).unwrap_or(JsonValue::Null);
                if let Err(err) = buffer
                    .append(self.message_id, wire.seq(), value, self.expires_at)
                    .await
                {
                    warn!(error = %err, message_id = %self.message_id,
                        "resume-buffer append failed (stream continues)");
                }
            }
        }
        if !self.client_gone && self.tx.send(event).await.is_err() {
            debug!(message_id = %self.message_id,
                "client disconnected; driver continues to completion (resume via Last-Event-ID)");
            self.client_gone = true;
        }
    }
}

/// Validated, owned request for `send_message`. Constructed by the handler
/// from the wire body + the JWT-derived [`Identity`].
#[domain_model]
#[derive(Debug, Clone)]
pub struct SendMessageRequest {
    pub session_id: Uuid,
    /// Ordered, typed body parts of the user message (FR-022). Must be
    /// non-empty — validated in `validate_request`.
    pub parts: Vec<MessagePartInput>,
    pub file_ids: Vec<Uuid>,
    pub parent_message_id: Option<Uuid>,
    pub capabilities: Option<Vec<CapabilityValue>>,
    /// Opaque per-turn client context persisted on the user message row
    /// (device, locale, active error state, …). Unlike `capabilities` it
    /// survives into reads, export, replay, recreate, and branching, so the
    /// conditioning of a past turn stays reconstructable. Validated by
    /// [`validate_message_metadata`].
    pub metadata: Option<JsonValue>,
}

/// Outgoing event stream returned by [`MessageService::send_message`]. The
/// handler maps each event to a single NDJSON line.
pub type SendMessageStream = BoxStream<'static, StreamingEvent>;

/// Outcome of a successful
/// [`MessageService::delete_message_cascade`] call. Mirrors the wire
/// payload of `DELETE /sessions/{session_id}/messages/{message_id}`.
#[domain_model]
#[derive(Debug, Clone)]
pub struct DeleteOutcome {
    /// Root of the deleted subtree (the message id from the request path).
    pub message_id: Uuid,
    /// Total messages removed by the cascade (target + descendants).
    pub deleted_count: u64,
    /// UTC commit timestamp captured immediately after the SERIALIZABLE
    /// transaction returned `Ok`. Wire-serialised as RFC-3339.
    pub deleted_at: OffsetDateTime,
}

/// Event kind dispatched to the backend plugin via
/// [`MessageService::dispatch_to_plugin`].
///
/// `New` triggers `ChatEngineBackendPlugin::on_message` (a normal send or
/// branch); `Recreate` triggers `on_message_recreate` (a sibling
/// regeneration). ADR-0013 explicitly distinguishes the two so plugins
/// can apply different prompting / sampling strategies.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEventKind {
    /// Normal new-message dispatch (`POST /messages/send` and the Phase 6
    /// branch endpoint).
    New,
    /// Sibling regeneration (`POST /messages/{id}/recreate`).
    Recreate,
}

/// Public service.
#[domain_model]
#[derive(Clone)]
pub struct MessageService {
    sessions: Arc<dyn SessionRepo>,
    session_types: Arc<dyn SessionTypeRepo>,
    messages: Arc<dyn MessageRepo>,
    plugins: PluginService,
    streaming_buffer_size: usize,
    plugin_deadline: Duration,
    /// Webhook sink used by Phase 12's `delete_message_cascade` to publish a
    /// `message.deleted` event after a successful commit. Phase 14 wires the
    /// production emitter; by default the service uses [`NoopWebhookEmitter`]
    /// so existing constructors stay source-compatible.
    webhooks: Arc<dyn WebhookEmitter>,
    /// Resume buffer (FR-024): when set, the driver tees every wire event into
    /// it (with `seq`) so a dropped connection can resume via `Last-Event-ID`.
    /// `None` disables buffering (the stream still works; just not resumable).
    stream_buffer: Option<Arc<dyn StreamEventBuffer>>,
    enforcer: PolicyEnforcer,
}

impl MessageService {
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
            streaming_buffer_size: DEFAULT_STREAMING_BUFFER_SIZE,
            plugin_deadline: DEFAULT_PLUGIN_DEADLINE,
            webhooks: Arc::new(NoopWebhookEmitter),
            stream_buffer: None,
            enforcer,
        }
    }

    /// Inject the resume buffer (FR-024). Defaults to `None` (no buffering) so
    /// existing constructors stay source-compatible.
    #[must_use]
    pub fn with_stream_buffer(mut self, buffer: Arc<dyn StreamEventBuffer>) -> Self {
        self.stream_buffer = Some(buffer);
        self
    }

    /// Inject the webhook emitter used by Phase 12's
    /// `delete_message_cascade`. Defaults to [`NoopWebhookEmitter`] when
    /// not set so existing call sites need not be updated.
    #[must_use]
    pub fn with_webhook_emitter(mut self, webhooks: Arc<dyn WebhookEmitter>) -> Self {
        self.webhooks = webhooks;
        self
    }

    /// Override the bounded-channel size used for the plugin-→-sink
    /// backpressure (ADR-0010). The default is conservative; operators
    /// may raise it once they've measured the typical chunk size.
    #[must_use]
    pub fn with_streaming_buffer_size(mut self, size: usize) -> Self {
        self.streaming_buffer_size = size.max(1);
        self
    }

    /// Override the per-call plugin deadline used to set
    /// [`PluginCallContext::deadline`].
    #[must_use]
    pub fn with_plugin_deadline(mut self, deadline: Duration) -> Self {
        self.plugin_deadline = deadline;
        self
    }

    /// Resolve a message by id and verify the caller owns its session,
    /// returning the message. Cross-tenant / unknown ids fold to a 404
    /// `message` not-found (anti-enumeration, ADR-0021). Used by the REST
    /// routes keyed on `message_id` only (`GET /messages/{id}`, recreate,
    /// reactions, variants) to both serve the message and resolve the
    /// owning `session_id` for session-scoped delegation.
    pub async fn resolve_owned_message(
        &self,
        ctx: &SecurityContext,
        message_id: Uuid,
    ) -> Result<Message> {
        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        // MESSAGE read: the PDP returns owner constraints; the scoped lookup
        // then both authorizes (denied → 403 via `?`) and resolves the row.
        // A 0-row scoped result hides existence from out-of-scope callers (404).
        let scope = self
            .enforcer
            .access_scope(
                ctx,
                &resource_types::MESSAGE,
                actions::READ,
                Some(message_id),
            )
            .await?;
        // The PDP scope alone clamps the tenant only; pin the caller's owner
        // pair into the SQL predicate so a same-tenant stranger's message
        // resolves to 0 rows (404) rather than being read.
        // @cpt-cf-chat-engine-nfr-authentication
        let scope = owner_guard::caller_scope(ctx, &scope);
        self.messages
            .find_message_by_id_scoped(&scope, message_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("message", message_id))
    }

    /// DELETE-authorized counterpart to [`Self::resolve_owned_message`] for the
    /// `message_id`-keyed delete route. Authorizes MESSAGE **delete** (not
    /// read) so a caller holding delete — but not read — permission is not
    /// spuriously denied, while preserving ownership and out-of-scope → 404
    /// (anti-enumeration, ADR-0021). Resolves the owning `session_id` for the
    /// session-scoped `delete_message_cascade` that follows.
    pub async fn resolve_owned_message_for_delete(
        &self,
        ctx: &SecurityContext,
        message_id: Uuid,
    ) -> Result<Message> {
        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        let scope = self
            .enforcer
            .access_scope(
                ctx,
                &resource_types::MESSAGE,
                actions::DELETE,
                Some(message_id),
            )
            .await?;
        // @cpt-cf-chat-engine-nfr-authentication
        let scope = owner_guard::caller_scope(ctx, &scope);
        self.messages
            .find_message_by_id_scoped(&scope, message_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("message", message_id))
    }

    /// List the active, visible conversation path of a session in
    /// chronological order (ownership-checked). When `parent_message_id` is
    /// supplied, only the direct replies under that node are returned.
    pub async fn list_active_messages(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        parent_message_id: Option<Uuid>,
    ) -> Result<Vec<Message>> {
        // @cpt-cf-chat-engine-seq-authz-list
        // @cpt-cf-chat-engine-interface-pep
        let scope = self
            .enforcer
            .access_scope(ctx, &resource_types::MESSAGE, actions::LIST, None)
            .await?;
        // Ownership travels with the scope into the `WHERE` clause: post-
        // filtering a page would leave the paging counts wrong.
        // @cpt-cf-chat-engine-nfr-authentication
        let scope = owner_guard::caller_scope(ctx, &scope);
        let messages = self
            .messages
            .fetch_active_history_scoped(&scope, session_id, None)
            .await?;
        Ok(match parent_message_id {
            Some(pid) => messages
                .into_iter()
                .filter(|m| m.parent_message_id == Some(pid))
                .collect(),
            None => messages,
        })
    }

    /// Process a new user message and return an NDJSON-ready
    /// `StreamingEvent` stream.
    ///
    /// On success the stream yields `Start → Chunk* → Complete`. On
    /// mid-stream failure it yields `Start → Chunk* → Error` and closes
    /// cleanly. On cancellation it terminates without emitting an extra
    /// event (per ADR-0008).
    ///
    /// Pre-stream failures (validation, plugin not found, plugin returns
    /// `Err` before any event) surface as `Err(ChatEngineError)` — the
    /// handler maps them to a JSON error body with the appropriate HTTP
    /// status. Once the stream is returned the HTTP response has already
    /// started; mid-stream failures stay on the wire and never produce an
    /// HTTP error.
    #[instrument(
        skip(self, req, ctx, cancel),
        fields(
            session_id = %req.session_id,
            user_id = %ctx.subject_id(),
            request_id,
            assistant_message_id,
        ),
    )]
    pub async fn send_message(
        &self,
        req: SendMessageRequest,
        ctx: &SecurityContext,
        cancel: CancellationToken,
    ) -> Result<SendMessageStream> {
        // Opaque tenant/user strings for plugin-call construction only; the
        // authorization boundary is the enforcer below.
        let identity = identity_from_ctx(ctx)?;

        // ---- 1. Authorize at parent-session granularity + validate payload. ----
        // A message send mutates the conversation, so it is gated as a SESSION
        // update; the child rows inherit the session's owner pair.
        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        let (session, _session_scope) = self
            .authorize_session(ctx, req.session_id, actions::UPDATE)
            .await?;
        let validated = self.validate_request(&req, &session).await?;

        // ---- 2. Atomic user-msg + assistant-stub insert. ----
        let InsertedPair {
            user_message_id,
            assistant_message_id,
            user_variant_index,
        } = self.pre_persist_user_message(&req, &identity).await?;

        tracing::Span::current().record(
            "assistant_message_id",
            tracing::field::display(assistant_message_id),
        );
        debug!(
            user_message_id = %user_message_id,
            assistant_message_id = %assistant_message_id,
            user_variant_index,
            "persisted user message + assistant stub"
        );

        // ---- 3. Build history & resolve plugin. ----
        let history = self
            .messages
            .fetch_active_history(req.session_id, None)
            .await?;

        let plugin = self.plugins.resolve(&validated.plugin_instance_id)?;
        let plugin_config = self
            .plugins
            .load_config(&validated.plugin_instance_id, validated.session_type_id)
            .await?;

        // ---- 4. Build plugin call ctx + invoke. ----
        let request_id = Uuid::new_v4();
        tracing::Span::current().record("request_id", tracing::field::display(request_id));

        // Child token from the handler-provided `cancel`. The plugin's
        // clone cancels when the parent cancels (connection close /
        // explicit cancellation); cancelling the child does NOT cancel
        // the parent so each request's lifetime stays independent.
        let plugin_cancel = cancel.child_token();
        let deadline = Instant::now() + self.plugin_deadline;

        let call_ctx = PluginCallContext {
            request_id,
            tenant_id: TenantId::new(identity.tenant_id.as_str()),
            user_id: UserId::new(identity.user_id.as_str()),
            plugin_instance_id: validated.plugin_instance_id.clone(),
            session_type_id: validated.session_type_id,
            plugin_config,
            enabled_capabilities: req.capabilities.clone(),
            deadline: Some(deadline),
            cancel: plugin_cancel.clone(),
        };

        let plugin_ctx = MessagePluginCtx {
            session_id: req.session_id,
            message_id: assistant_message_id,
            messages: history,
            call_ctx,
        };

        // Pre-stream plugin failure → finalize stub with error metadata
        // and return mapped HTTP status to the handler.
        let plugin_stream = match plugin.on_message(plugin_ctx).await {
            Ok(s) => s,
            Err(err) => {
                let finish_reason = finish_reason_for(&err);
                self.messages
                    .finalize_assistant(
                        req.session_id,
                        assistant_message_id,
                        FinalizeOutcome::Errored {
                            text: String::new(),
                            error: err.to_string(),
                            finish_reason,
                        },
                    )
                    .await
                    .ok();
                return Err(err.into());
            }
        };

        // ---- 5. Spawn driver task + return bounded-channel-backed stream. ----
        let messages_repo = Arc::clone(&self.messages);
        let overflow_ctx = OverflowDispatchCtx {
            service: self.clone(),
            sessions: Arc::clone(&self.sessions),
            session_id: req.session_id,
            tenant_id: identity.tenant_id.clone(),
            user_id: identity.user_id.clone(),
        };
        let stream = self.spawn_driver(
            req.session_id,
            assistant_message_id,
            plugin_stream,
            messages_repo,
            cancel,
            plugin_cancel,
            deadline,
            Some(overflow_ctx),
        );

        info!(
            request_id = %request_id,
            assistant_message_id = %assistant_message_id,
            "send_message dispatch successful \u{2014} streaming response"
        );

        Ok(stream)
    }

    /// Phase 7: build the `Vec<Message>` payload sent to a plugin from a
    /// session's active path under the session's current memory strategy.
    ///
    /// Algorithm summary (see ADR-0017 + the Phase 7 feature spec):
    /// - [`MemoryStrategy::Full`]: include every active-path message with
    ///   `is_hidden_from_backend=false`, then append `current_msg`.
    /// - [`MemoryStrategy::SlidingWindow`]: take the last `window_size`
    ///   visible (i.e., `is_hidden_from_backend=false`) active-path
    ///   messages, then append `current_msg`.
    /// - [`MemoryStrategy::Summarized`]: include every visible active-path
    ///   message AND the last `recent_messages_to_keep` active-path messages
    ///   regardless of visibility (deduplicated), then append `current_msg`.
    ///
    /// `current_msg` is appended verbatim — callers control whether the
    /// just-persisted user message belongs in history or is a synthetic
    /// "next prompt" stub. Order of the returned vector is `created_at` ASC
    /// for the active-path slice; `current_msg` is the final element.
    pub async fn apply_memory_strategy(
        &self,
        session: &Session,
        current_msg: &Message,
    ) -> Result<Vec<Message>> {
        let meta_value = session.metadata.clone().unwrap_or(JsonValue::Null);
        let strategy = read_memory_strategy(&meta_value);

        let active = self.messages.list_active_path(session.session_id).await?;

        let mut out: Vec<Message> = match &strategy {
            MemoryStrategy::Full => active
                .into_iter()
                .filter(|m| !m.is_hidden_from_backend)
                .collect(),
            MemoryStrategy::SlidingWindow { window_size } => {
                let visible: Vec<Message> = active
                    .into_iter()
                    .filter(|m| !m.is_hidden_from_backend)
                    .collect();
                let n = *window_size as usize;
                let start = visible.len().saturating_sub(n);
                visible[start..].to_vec()
            }
            MemoryStrategy::Summarized {
                recent_messages_to_keep,
            } => {
                let keep = *recent_messages_to_keep as usize;
                let total = active.len();
                let recent_start = total.saturating_sub(keep);
                let mut acc: Vec<Message> = Vec::with_capacity(total);
                for (idx, m) in active.iter().enumerate() {
                    let keep_recent = idx >= recent_start;
                    if keep_recent || !m.is_hidden_from_backend {
                        acc.push(m.clone());
                    }
                }
                acc
            }
        };
        out.push(current_msg.clone());
        Ok(out)
    }

    /// Phase 7 / 8: dispatch entrypoint for context-overflow recovery.
    ///
    /// Called by the streaming-error path when a plugin emits
    /// `StreamingErrorEvent { error: "context_overflow: ..." }`.
    ///
    /// - For [`MemoryStrategy::Full`] / [`MemoryStrategy::SlidingWindow`]:
    ///   propagate the original error (no automatic recovery).
    /// - For [`MemoryStrategy::Summarized`] (Phase 8): invoke
    ///   `ChatEngineBackendPlugin::on_session_summary`, persist the
    ///   resulting summary, and flip the reported `summarized_message_ids`
    ///   to `is_hidden_from_backend=true` so the NEXT message-send
    ///   request observes the compressed history. Returns `Ok(())` on
    ///   successful recovery installation; returns `Err(BackendUnavailable)`
    ///   when the plugin call fails so a re-occurrence (Phase 7 driver:
    ///   already finalised on the wire) is properly observable.
    ///
    /// At-most-once semantics: the driver only invokes
    /// `handle_context_overflow` from the *first* streaming-error
    /// observation per request. A second overflow on the next message
    /// request will fire this hook again — but by then the summary is
    /// already installed, so the plugin's prompt is much smaller and
    /// the retry succeeds. If the plugin still rejects (truly oversized
    /// recent_messages_to_keep), the error propagates to the client
    /// unchanged.
    pub async fn handle_context_overflow(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
        current_strategy: &MemoryStrategy,
    ) -> Result<()> {
        info!(
            session_id = %session_id,
            strategy_type = %strategy_type_label(current_strategy),
            "context_overflow observed \u{2014} dispatching to overflow handler",
        );

        match current_strategy {
            MemoryStrategy::Summarized { .. } => {
                self.recover_via_session_summary(tenant_id, user_id, session_id)
                    .await
            }
            MemoryStrategy::Full | MemoryStrategy::SlidingWindow { .. } => {
                Err(ChatEngineError::BackendUnavailable {
                    reason: "context_overflow: backend rejected request as oversized".to_string(),
                    retry_after: None,
                    source: None,
                })
            }
        }
    }

    /// Phase 8 implementation of the Summarized branch of
    /// [`Self::handle_context_overflow`]. Loads the session via the
    /// **scoped** lookup, resolves its backend plugin, invokes
    /// `on_session_summary`, drains the stream (the original HTTP body
    /// has already closed by the time the driver task fires this), and
    /// atomically persists the summary message plus flips the reported
    /// `summarized_message_ids` to `is_hidden_from_backend=true` so the
    /// NEXT message-send request observes the compressed history.
    ///
    /// `tenant_id` / `user_id` are threaded through from the driver
    /// task's captured identity (see `OverflowDispatchCtx`) so this
    /// method can use the scoped `find_by_id` — the prior version
    /// reached for `find_by_session_id_unscoped` and relied on the
    /// driver's *previous* scoped fetch validating ownership, which is
    /// a per-caller convention rather than a repo-level guarantee.
    async fn recover_via_session_summary(
        &self,
        tenant_id: &str,
        user_id: &str,
        session_id: Uuid,
    ) -> Result<()> {
        let Some(row) = self
            .sessions
            .find_by_id(tenant_id, user_id, session_id)
            .await?
        else {
            warn!(
                session_id = %session_id,
                "context_overflow recovery skipped: session row not accessible \
                 under the calling identity's scope",
            );
            return Ok(());
        };
        let session_type_id =
            row.session_type_id
                .ok_or_else(|| ChatEngineError::BackendUnavailable {
                    reason: "context_overflow recovery: session has no session_type bound"
                        .to_string(),
                    retry_after: None,
                    source: None,
                })?;
        let st = self
            .session_types
            .find_by_id(session_type_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("session_type", session_type_id))?;
        let plugin_instance_id =
            st.plugin_instance_id
                .ok_or_else(|| ChatEngineError::BackendUnavailable {
                    reason: "context_overflow recovery: session_type has no plugin binding"
                        .to_string(),
                    retry_after: None,
                    source: None,
                })?;
        let plugin = self.plugins.resolve(&plugin_instance_id)?;
        let plugin_config = self
            .plugins
            .load_config(&plugin_instance_id, session_type_id)
            .await?;

        let cancel = CancellationToken::new();
        let deadline = Instant::now() + self.plugin_deadline;
        let call_ctx = PluginCallContext {
            request_id: Uuid::new_v4(),
            tenant_id: TenantId::new(row.tenant_id.as_str()),
            user_id: UserId::new(row.user_id.as_str()),
            plugin_instance_id: plugin_instance_id.clone(),
            session_type_id,
            plugin_config,
            enabled_capabilities: None,
            deadline: Some(deadline),
            cancel: cancel.clone(),
        };
        let plugin_ctx = SessionPluginCtx {
            session_type_id,
            session_id: Some(session_id),
            call_ctx,
        };

        // Invoke the plugin. A pre-stream error propagates as
        // BackendUnavailable per the standard PluginError → ChatEngineError
        // mapping.
        let mut summary_stream = plugin.on_session_summary(plugin_ctx).await?;
        // `cancel` keeps the token alive for the duration of the stream so
        // the deadline guard (if any) does not fire on a token that has
        // already been dropped.
        let _cancel_guard = cancel;

        let mut accumulator = String::new();
        let mut metadata: Option<JsonValue> = None;
        let mut summarized_ids: Vec<Uuid> = Vec::new();

        while let Some(item) = summary_stream.next().await {
            match item {
                Ok(StreamingEvent::Start(_)) => {}
                Ok(StreamingEvent::Chunk(c)) => accumulator.push_str(&c.chunk),
                Ok(StreamingEvent::Complete(c)) => {
                    if let Some(ref m) = c.metadata {
                        summarized_ids = extract_summarized_ids_from_meta(m);
                    }
                    metadata = c.metadata;
                    break;
                }
                Ok(StreamingEvent::Error(e)) => {
                    return Err(ChatEngineError::BackendUnavailable {
                        reason: format!(
                            "context_overflow recovery: on_session_summary errored: {}",
                            e.error
                        ),
                        retry_after: None,
                        source: None,
                    });
                }
                // Internal context-overflow summary is text-only; richer events
                // (status/part/citation/state/session-meta/tool) are irrelevant
                // to summary persistence and are ignored here.
                Ok(_) => {}
                Err(err) => {
                    return Err(err.into());
                }
            }
        }

        // Persist the summary message + flip the reported ids in a
        // single SERIALIZABLE transaction (see
        // `MessageRepo::insert_summary_message`).
        self.messages
            .insert_summary_message(
                session_id,
                accumulator,
                metadata,
                summarized_ids,
                Some(tenant_id.to_owned()),
            )
            .await?;

        info!(
            session_id = %session_id,
            "context_overflow recovery installed (Phase 8): summary persisted",
        );
        Ok(())
    }

    /// Phase 7: persist a new [`MemoryStrategy`] under
    /// `session.metadata["memory_strategy"]`.
    ///
    /// The call is the service-side of `PATCH /sessions/{id}` body field
    /// `memory_strategy`. It:
    ///
    /// 1. Validates the strategy via [`validate_memory_strategy`]
    ///    (`400 Bad Request` on invalid bounds).
    /// 2. Loads the session row scoped to `identity` (`404 Not Found` if
    ///    the row is not owned by the caller).
    /// 3. Rejects updates against sessions in `SoftDeleted` /
    ///    `HardDeleted` lifecycle states with `409 Conflict`. `Active` and
    ///    `Archived` are allowed.
    /// 4. Merges the strategy into the existing metadata object via
    ///    [`write_memory_strategy`] (sibling keys preserved verbatim).
    /// 5. Persists the merged metadata via `SessionRepo::update_metadata`
    ///    — a single statement, so the strategy write is atomic.
    ///
    /// On success the next call to [`Self::apply_memory_strategy`] reads
    /// the new value (no session-state restart required).
    pub async fn update_memory_strategy(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        strategy: MemoryStrategy,
    ) -> Result<Session> {
        validate_memory_strategy(&strategy)?;

        // memory_strategy lives in session.metadata, so this is a SESSION update.
        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        let (row, scope) = self
            .authorize_session(ctx, session_id, actions::UPDATE)
            .await?;

        let state = row.lifecycle_state;
        if matches!(
            state,
            LifecycleState::SoftDeleted | LifecycleState::HardDeleted
        ) {
            return Err(ChatEngineError::conflict(format!(
                "session is {state} and cannot accept memory_strategy updates",
            )));
        }

        let mut meta = row.metadata.clone().unwrap_or(JsonValue::Null);
        write_memory_strategy(&mut meta, &strategy);

        let updated = self
            .sessions
            .update_metadata_scoped(&scope, session_id, Some(meta))
            .await?;

        // Strip reserved metadata + clear the share token before handing
        // the session back to the caller (Phase 14 DTO mapping mirrors the
        // same redaction; we apply it here so direct callers of the
        // service surface get the same shape).
        Ok(redact_session(updated))
    }

    /// Phase 6: pre-allocate a fresh assistant variant sibling under
    /// `parent_message_id`.
    ///
    /// Strategy mirrors `pre_persist_user_message` for the new-send flow
    /// but tailored for **recreate** semantics (ADR-0013):
    ///   - The new row is an *assistant* sibling of an existing message —
    ///     it has the same `parent_message_id` as the target user message
    ///     (recreate's `target.parent_message_id`, per the feature spec).
    ///   - The `variant_index` is computed inside the SAME SERIALIZABLE
    ///     transaction as the INSERT (via
    ///     `infra::db::compute_next_variant_index`); the whole pair is
    ///     retried up to 3 times on `uq_messages_session_parent_variant`
    ///     collisions, with exhaustion mapping to HTTP 409.
    ///   - `is_active=true` on the new row — the variant becomes the
    ///     active leaf for that parent. Older siblings are deactivated by
    ///     [`VariantService::update_active_path`] *after* the stream
    ///     completes (active-path consistency, ADR-0011).
    ///   - `is_complete=false` (the streaming pipeline will flip this
    ///     bit via `finalize_assistant` once the plugin closes).
    ///
    /// Returns an [`InsertedPair`] re-used as `(parent, new_assistant,
    /// new_variant_index)` so the variant service can immediately
    /// reference the new message id in the outgoing `Start` event.
    /// `user_message_id` is reused as the parent id for ergonomic API.
    pub async fn prepare_recreate_stub(
        &self,
        session_id: Uuid,
        parent_message_id: Uuid,
        tenant_id: Option<String>,
    ) -> Result<InsertedPair> {
        self.messages
            .insert_assistant_variant_stub(session_id, parent_message_id, tenant_id)
            .await
    }

    /// Streaming dispatch reusable by both the new-send path and the
    /// Phase-6 recreate / branch paths.
    ///
    /// The caller is responsible for:
    ///   - persisting the parent context (user message + assistant stub)
    ///     before calling — `assistant_message_id` MUST already exist;
    ///   - building the `history` that is shipped to the plugin (the
    ///     history shape differs between New and Recreate);
    ///   - resolving `plugin_instance_id` + `session_type_id` from the
    ///     session row.
    ///
    /// Returns the NDJSON-ready `SendMessageStream`. The driver task is
    /// spawned internally; cancellation flows through `cancel`.
    #[allow(clippy::too_many_arguments)]
    pub async fn dispatch_to_plugin(
        &self,
        identity: &Identity,
        session_id: Uuid,
        session_type_id: Uuid,
        plugin_instance_id: String,
        assistant_message_id: Uuid,
        history: Vec<Message>,
        capabilities: Option<Vec<CapabilityValue>>,
        event_kind: MessageEventKind,
        cancel: CancellationToken,
    ) -> Result<SendMessageStream> {
        let plugin = self.plugins.resolve(&plugin_instance_id)?;
        let plugin_config = self
            .plugins
            .load_config(&plugin_instance_id, session_type_id)
            .await?;

        let request_id = Uuid::new_v4();
        let plugin_cancel = cancel.child_token();
        let deadline = Instant::now() + self.plugin_deadline;

        let call_ctx = PluginCallContext {
            request_id,
            tenant_id: TenantId::new(identity.tenant_id.as_str()),
            user_id: UserId::new(identity.user_id.as_str()),
            plugin_instance_id: plugin_instance_id.clone(),
            session_type_id,
            plugin_config,
            enabled_capabilities: capabilities,
            deadline: Some(deadline),
            cancel: plugin_cancel.clone(),
        };

        let plugin_ctx = MessagePluginCtx {
            session_id,
            message_id: assistant_message_id,
            messages: history,
            call_ctx,
        };

        // Dispatch the request to the plugin per the event kind. The
        // SDK exposes `on_message` for `New` and `on_message_recreate`
        // for `Recreate` (ADR-0013).
        let plugin_stream = match event_kind {
            MessageEventKind::New => plugin.on_message(plugin_ctx).await,
            MessageEventKind::Recreate => plugin.on_message_recreate(plugin_ctx).await,
        };

        let plugin_stream = match plugin_stream {
            Ok(s) => s,
            Err(err) => {
                let finish_reason = finish_reason_for(&err);
                self.messages
                    .finalize_assistant(
                        session_id,
                        assistant_message_id,
                        FinalizeOutcome::Errored {
                            text: String::new(),
                            error: err.to_string(),
                            finish_reason,
                        },
                    )
                    .await
                    .ok();
                return Err(err.into());
            }
        };

        let messages_repo = Arc::clone(&self.messages);
        let overflow_ctx = OverflowDispatchCtx {
            service: self.clone(),
            sessions: Arc::clone(&self.sessions),
            session_id,
            tenant_id: identity.tenant_id.clone(),
            user_id: identity.user_id.clone(),
        };
        let stream = self.spawn_driver(
            session_id,
            assistant_message_id,
            plugin_stream,
            messages_repo,
            cancel,
            plugin_cancel,
            deadline,
            Some(overflow_ctx),
        );

        info!(
            request_id = %request_id,
            assistant_message_id = %assistant_message_id,
            event_kind = ?event_kind,
            "dispatch_to_plugin successful \u{2014} streaming response"
        );

        Ok(stream)
    }

    /// Cancellation primitive Phase 12 will surface as
    /// `DELETE /sessions/{id}/messages/{id}/streaming`. Cancelling an
    /// in-flight call cancels the corresponding `CancellationToken` passed
    /// into [`send_message`]; the rest of the pipeline (partial persist,
    /// stream close) happens automatically inside the driver task.
    ///
    /// In Phase 5 this is a free function the handler can call directly;
    /// Phase 12 will wrap it in a request-routed handler.
    pub fn cancel_streaming(cancel: &CancellationToken) {
        cancel.cancel();
    }

    /// Phase 12: cascade-delete a message subtree.
    ///
    /// Validates ownership against the JWT-derived [`Identity`], refuses to
    /// delete the session's root message, then delegates to
    /// [`MessageRepo::delete_message_subtree`] (the canonical Phase 8
    /// SERIALIZABLE primitive that removes the target message plus every
    /// descendant). Reactions cascade automatically via the
    /// `message_reactions.message_id` FK created in Phase 1.
    ///
    /// On success the method emits a fire-and-forget
    /// [`WebhookEvent::MessageDeleted`] event AFTER the transaction commits
    /// — webhook-delivery failures are logged at debug level and never
    /// roll back the DB write.
    ///
    /// ## Inputs
    /// - `identity` — JWT-derived `(tenant_id, user_id)`. The service
    ///   MUST NOT accept these values from any other source (PRD §7).
    /// - `session_id` — owning session.
    /// - `message_id` — root of the subtree to delete.
    ///
    /// ## Returns
    /// [`DeleteOutcome`] carrying `message_id`, `deleted_count` (target +
    /// descendants), and the UTC RFC-3339 commit timestamp.
    ///
    /// ## Errors
    /// - [`ChatEngineError::Forbidden`] (HTTP 403) — session row exists
    ///   but belongs to a different tenant.
    /// - [`ChatEngineError::NotFound`] (HTTP 404) — session row absent,
    ///   owned by a different user inside the same tenant (anti-
    ///   enumeration), or message row absent from `session_id`. Idempotent
    ///   re-delete also lands here.
    /// - [`ChatEngineError::Conflict`] (HTTP 409) — caller targeted the
    ///   session's root message (`parent_message_id IS NULL`). No DB
    ///   mutation occurs.
    /// - [`ChatEngineError::Internal`] / [`ChatEngineError::BadRequest`]
    ///   — propagated from the underlying repo or identity construction.
    #[instrument(
        skip(self, ctx),
        fields(
            session_id = %session_id,
            message_id = %message_id,
            user_id = %ctx.subject_id(),
            deleted_count,
        ),
    )]
    pub async fn delete_message_cascade(
        &self,
        ctx: &SecurityContext,
        session_id: Uuid,
        message_id: Uuid,
    ) -> Result<DeleteOutcome> {
        // Opaque tenant/user strings for the post-commit webhook only.
        let identity = identity_from_ctx(ctx)?;

        // 1. MESSAGE delete decision. The PDP returns owner constraints; a
        //    denied decision fails closed to 403 via `?`. The compiled scope
        //    then bounds every read/delete below to the caller's owned rows,
        //    so a foreign or cross-scope target simply resolves to 0 rows (404)
        //    — anti-enumeration (ADR-0021) without a separate scope probe.
        // @cpt-cf-chat-engine-seq-authz-point-op
        // @cpt-cf-chat-engine-interface-pep
        let scope = self
            .enforcer
            .access_scope(
                ctx,
                &resource_types::MESSAGE,
                actions::DELETE,
                Some(message_id),
            )
            .await?;
        // @cpt-cf-chat-engine-nfr-authentication
        let scope = owner_guard::caller_scope(ctx, &scope);

        // 2. Resolve the target message scoped to `session_id` under the PDP
        //    scope. A miss folds to 404 — this also covers the idempotent
        //    re-delete case (the previous call already removed the subtree).
        let target = self
            .messages
            .find_message_in_session_scoped(&scope, session_id, message_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("message", message_id))?;

        // 3. Root-message guard. A root message has no parent — removing
        //    it would obliterate the entire session via the cascade. Per
        //    the spec this is rejected as 409 BEFORE any DB write.
        if target.parent_message_id.is_none() {
            return Err(ChatEngineError::conflict(
                "cannot delete the root message of a session",
            ));
        }

        // 4. Atomic SERIALIZABLE cascade. The Phase 8 primitive walks the
        //    subtree, deletes leaves-first to satisfy the
        //    `messages.parent_message_id` FK, and lets the
        //    `message_reactions.message_id` FK CASCADE drop reactions in
        //    the same transaction.
        let deleted_count = self
            .messages
            .delete_message_subtree_scoped(&scope, session_id, message_id)
            .await?;

        // Concurrent re-delete: if a parallel request removed the subtree
        // between our `find_message_in_session` lookup and the cascade
        // call, the repo returns `Ok(0)`. Surface that as 404 so the
        // wire contract stays single-source — the spec calls for
        // idempotent 404 on missing targets.
        if deleted_count == 0 {
            return Err(ChatEngineError::not_found("message", message_id));
        }

        // 5. Commit timestamp captured AFTER the DB write returns Ok.
        let deleted_at = OffsetDateTime::now_utc();

        tracing::Span::current().record("deleted_count", tracing::field::display(deleted_count));
        info!(
            session_id = %session_id,
            message_id = %message_id,
            deleted_count,
            "message subtree deleted",
        );

        // 6. Fire-and-forget webhook emission. Follows the Phase 9
        //    reaction-notification pattern: detached `tokio::spawn`,
        //    debug! on emitter failure, never propagates upstream.
        let webhooks = Arc::clone(&self.webhooks);
        let event = WebhookEvent::MessageDeleted {
            session_id,
            message_id,
            tenant_id: identity.tenant_id.clone(),
            user_id: identity.user_id.clone(),
            deleted_count,
            deleted_at,
        };
        tokio::spawn(async move {
            if let Err(err) = webhooks.emit(event).await {
                debug!(
                    target: "chat_engine::message::delete",
                    error = %err,
                    "message.deleted webhook emission failed (swallowed)",
                );
            }
        });

        Ok(DeleteOutcome {
            message_id,
            deleted_count,
            deleted_at,
        })
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    /// Trusted prefetch of a session by id, then the PDP decision for `action`.
    /// Mirrors `SessionService::authorize_session_op`: returns the prefetched
    /// row (source of the owner pair passed to the PDP as ABAC input) plus the
    /// compiled scope. A denied / unreachable / uncompilable PDP decision fails
    /// closed to `Forbidden` via the `?`-converted `EnforcerError`
    /// (DESIGN §3.5.5).
    // @cpt-cf-chat-engine-seq-authz-point-op
    async fn authorize_session(
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

        // Ownership is a gear invariant, not a PDP outcome: no shipped policy
        // plugin constrains `owner_id`, so a tenant-only scope would admit a
        // same-tenant stranger. Check the owner pair on the trusted prefetch
        // before the decision; the PDP may only narrow from here.
        // @cpt-cf-chat-engine-nfr-authentication
        owner_guard::ensure_session_owner(ctx, &prefetch)?;

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
        Ok((prefetch, scope))
    }

    /// Authentication, ownership, lifecycle, and payload validation.
    /// Returns the resolved session-type fields the streaming pipeline
    /// needs (`session_type_id`, `plugin_instance_id`).
    #[instrument(skip(self, req, session), fields(session_id = %req.session_id))]
    async fn validate_request(
        &self,
        req: &SendMessageRequest,
        session: &Session,
    ) -> Result<ValidatedRequest> {
        if req.parts.is_empty() {
            return Err(ChatEngineError::bad_request(
                "message must have at least one part",
            ));
        }

        validate_message_metadata(req.metadata.as_ref())?;

        // Ownership is already enforced by the caller's parent-session PDP
        // decision (`authorize_session`); `session` is the authorized row.

        // Lifecycle state must allow new messages — only Active does.
        let state = session.lifecycle_state;
        if !matches!(state, LifecycleState::Active) {
            return Err(ChatEngineError::conflict(format!(
                "session is {state} and does not accept new messages",
            )));
        }

        // Parent must belong to the same session.
        if let Some(parent_id) = req.parent_message_id {
            let exists = self
                .messages
                .find_message_in_session(req.session_id, parent_id)
                .await?;
            if exists.is_none() {
                return Err(ChatEngineError::bad_request(
                    "parent_message_id does not exist in this session",
                ));
            }
        }

        // file_ids: we trust the wire-level UUID parse — additional v4
        // discrimination is intentionally NOT performed because most
        // clients legitimately mint UUIDs from v1/v7 sources. Per the
        // spec, Chat Engine never fetches the content; an invalid id is
        // a downstream concern for the file service that owns it.
        // (Format validation already happened at the JSON layer.)

        // Capabilities must be a subset of the session's enabled set.
        if let Some(ref requested) = req.capabilities {
            let allowed_names =
                capability_names_from_session(session.enabled_capabilities.as_ref());
            for cap in requested {
                if !allowed_names.contains(&cap.name) {
                    return Err(ChatEngineError::bad_request(format!(
                        "capability '{}' is not enabled for this session",
                        cap.name
                    )));
                }
            }
        }

        // Session-type + plugin binding are required for message routing.
        let session_type_id = session.session_type_id.ok_or_else(|| {
            ChatEngineError::bad_request(
                "session has no session_type bound; messages cannot be routed",
            )
        })?;
        let session_type = self
            .session_types
            .find_by_id(session_type_id)
            .await?
            .ok_or_else(|| ChatEngineError::not_found("session_type", session_type_id))?;
        let plugin_instance_id = session_type.plugin_instance_id.ok_or_else(|| {
            ChatEngineError::bad_request(
                "session_type has no plugin_instance_id; messages cannot be routed",
            )
        })?;

        Ok(ValidatedRequest {
            session_type_id,
            plugin_instance_id,
        })
    }

    /// Atomic SERIALIZABLE insert of the user message + the assistant
    /// stub. The matching `variant_index` is computed inline (via
    /// `infra::db::compute_next_variant_index`) within the SAME
    /// transaction as the INSERT, with the whole pair retried under
    /// `VARIANT_INDEX_MAX_RETRIES` on
    /// `uq_messages_session_parent_variant` collisions — the previous
    /// `assign_variant_index` helper had its own transaction for the
    /// SELECT and left a race window before the INSERT.
    #[instrument(skip(self, req, identity), fields(session_id = %req.session_id))]
    async fn pre_persist_user_message(
        &self,
        req: &SendMessageRequest,
        identity: &Identity,
    ) -> Result<InsertedPair> {
        let payload = NewUserMessage {
            session_id: req.session_id,
            // Denormalized owning tenant + authoring user, sourced from the JWT
            // identity (never the request body). The session is already proven
            // to belong to this identity by `validate_request`, so the session's
            // `tenant_id` and `identity.tenant_id` are equal.
            tenant_id: Some(identity.tenant_id.clone()),
            user_id: Some(identity.user_id.clone()),
            parent_message_id: req.parent_message_id,
            parts: req.parts.clone(),
            file_ids: if req.file_ids.is_empty() {
                None
            } else {
                Some(req.file_ids.clone())
            },
            metadata: req.metadata.clone(),
        };
        self.messages.insert_user_and_assistant_stub(payload).await
    }

    /// Build the consumer-facing event stream and spawn the driver task
    /// that pumps the plugin's `PluginStream` into a bounded channel.
    ///
    /// Backpressure (ADR-0010): the channel is bounded. When full the
    /// driver blocks on `tx.send(...)` so the plugin stops producing — no
    /// chunks are dropped. The driver also `select!`s on
    /// `cancel.cancelled()` so connection close terminates the pipeline
    /// promptly.
    fn spawn_driver(
        &self,
        session_id: Uuid,
        assistant_id: Uuid,
        mut plugin_stream: PluginStream,
        messages: Arc<dyn MessageRepo>,
        // Parent (handler) cancellation token — cancelled by axum's
        // connection-close future or by Phase 12's explicit DELETE.
        cancel: CancellationToken,
        // Child token threaded into `PluginCallContext` — cancelling it
        // also tells the plugin to stop.
        plugin_cancel: CancellationToken,
        // Absolute deadline (mirrors `PluginCallContext.deadline`). When
        // it fires we cancel the plugin token so plugins that only
        // observe `cancel.cancelled()` still abort.
        deadline: Instant,
        // Phase 7: optional overflow-dispatch context. When `Some`, the
        // driver fires `MessageService::handle_context_overflow` once it
        // sees a `context_overflow:` streaming error. `None` disables the
        // hook (used by test fixtures that don't need it).
        overflow_ctx: Option<OverflowDispatchCtx>,
    ) -> SendMessageStream {
        let (tx, rx) = mpsc::channel::<StreamingEvent>(self.streaming_buffer_size);

        // Sleep-until-deadline guard. When the deadline fires we cancel
        // the plugin token; the driver loop then folds the elapsed
        // deadline into a timeout error (downstream of the channel).
        let plugin_cancel_for_deadline = plugin_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            plugin_cancel_for_deadline.cancel();
        });

        let tx_for_driver = tx.clone();
        // Resume-buffer tee (FR-024): every event the driver emits is also
        // projected + appended to the buffer with a monotonic `seq`.
        let stream_buffer = self.stream_buffer.clone();
        let buffer_expires_at = OffsetDateTime::now_utc() + RESUME_BUFFER_TTL;
        tokio::spawn(async move {
            let mut emitter = Emitter {
                tx: tx_for_driver,
                projector: DeltaProjector::new(),
                buffer: stream_buffer,
                message_id: assistant_id,
                expires_at: buffer_expires_at,
                client_gone: false,
            };
            // 1) Emit Start. A disconnected client (failed send) does not abort
            // the driver — it runs to completion so a reconnect can resume.
            let start = StreamingEvent::Start(StreamingStartEvent {
                message_id: assistant_id,
            });
            emitter.emit(start).await;

            let mut accumulator = String::new();
            let mut last_metadata: Option<JsonValue> = None;
            // FR-024 Phase B accumulators for the richer event vocabulary:
            // parts streamed via `Part`, mid-stream citations folded onto the
            // primary text part, the last `State` patch, tool traces, and the
            // merged `SessionMeta` patch (applied to the session at finalize).
            let mut extra_parts: Vec<MessagePartInput> = Vec::new();
            let mut text_citations = PartCitations::default();
            let mut state_patch: Option<JsonValue> = None;
            let mut tool_traces: Vec<JsonValue> = Vec::new();
            let mut session_patch = serde_json::Map::new();
            // Session-persist handles (sessions repo + identity) for applying a
            // streamed `SessionMeta` patch. Cloned from the overflow ctx so its
            // own hook can still consume `overflow_ctx` later.
            let session_persist = overflow_ctx.as_ref().map(|c| {
                (
                    Arc::clone(&c.sessions),
                    c.tenant_id.clone(),
                    c.user_id.clone(),
                    c.session_id,
                )
            });
            // Outcome the driver settles on if the parent token is cancelled
            // (explicit stop) before the plugin terminates. A client *drop* no
            // longer reaches here — only an explicit `cancel.cancel()` does.
            let mut outcome = DriverOutcome::CancelledByClient;
            // Phase 7: flips to `true` when the plugin emits
            // `StreamingErrorEvent { error: "context_overflow: ..." }`. The
            // post-loop dispatch then calls `handle_context_overflow`. The
            // flag is intentionally a single-shot bool — at-most-once per
            // inbound request, per the Phase 7 spec.
            let mut overflow_observed = false;

            loop {
                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => {
                        // Parent token cancelled — connection close /
                        // explicit DELETE. Propagate to the plugin and
                        // exit. The plugin should observe the signal via
                        // its child clone and stop producing.
                        plugin_cancel.cancel();
                        // `outcome` already initialised to `CancelledByClient`.
                        break;
                    }

                    next = plugin_stream.next() => {
                        let Some(item) = next else {
                            // Plugin closed its stream without emitting
                            // `Complete`. Treat as a graceful end.
                            outcome = DriverOutcome::Completed {
                                metadata: last_metadata.clone(),
                                citations: PartCitations::default(),
                            };
                            break;
                        };

                        match item {
                            Ok(StreamingEvent::Start(_)) => {
                                // Plugins may emit their own Start; we
                                // already wrote ours so drop the dup.
                            }
                            Ok(StreamingEvent::Chunk(c)) => {
                                accumulator.push_str(&c.chunk);
                                // Re-emit with the canonical assistant
                                // id so the wire is always consistent.
                                let evt = StreamingEvent::Chunk(StreamingChunkEvent {
                                    message_id: assistant_id,
                                    chunk: c.chunk,
                                });
                                // A dropped client connection is non-fatal: the
                                // driver keeps consuming + buffering so a
                                // reconnect resumes the full response.
                                emitter.emit(evt).await;
                            }
                            // FR-024 vocabulary events: accumulate for
                            // persistence, then stream + buffer for resume.
                            Ok(StreamingEvent::Status(s)) => {
                                // Transient progress — streamed, never persisted.
                                emitter.emit(StreamingEvent::Status(s)).await;
                            }
                            Ok(StreamingEvent::Part(p)) => {
                                extra_parts.push(p.part.clone());
                                emitter.emit(StreamingEvent::Part(p)).await;
                            }
                            Ok(StreamingEvent::Citation(c)) => {
                                // Mid-stream citations fold onto the primary text
                                // part (part 0); parts streamed via `Part` carry
                                // their own citations.
                                text_citations.file_citations.extend(c.file_citations.clone());
                                text_citations.link_citations.extend(c.link_citations.clone());
                                text_citations.references.extend(c.references.clone());
                                emitter.emit(StreamingEvent::Citation(c)).await;
                            }
                            Ok(StreamingEvent::State(s)) => {
                                state_patch = Some(s.state.clone());
                                emitter.emit(StreamingEvent::State(s)).await;
                            }
                            Ok(StreamingEvent::SessionMeta(s)) => {
                                if let Some(obj) = s.patch.as_object() {
                                    for (k, v) in obj {
                                        session_patch.insert(k.clone(), v.clone());
                                    }
                                }
                                emitter.emit(StreamingEvent::SessionMeta(s)).await;
                            }
                            Ok(StreamingEvent::Tool(t)) => {
                                tool_traces.push(serde_json::json!({
                                    "tool": t.tool, "payload": t.payload,
                                }));
                                emitter.emit(StreamingEvent::Tool(t)).await;
                            }
                            Ok(StreamingEvent::Complete(c)) => {
                                last_metadata = c.metadata.clone();
                                // Merge the plugin's complete-time citations onto
                                // any accumulated mid-stream citations for the
                                // primary text part (FR-023 + FR-024 Phase B).
                                text_citations.file_citations.extend(c.file_citations.clone());
                                text_citations.link_citations.extend(c.link_citations.clone());
                                text_citations.references.extend(c.references.clone());
                                let evt = StreamingEvent::Complete(StreamingCompleteEvent {
                                    message_id: assistant_id,
                                    metadata: c.metadata,
                                    file_citations: c.file_citations,
                                    link_citations: c.link_citations,
                                    references: c.references,
                                });
                                emitter.emit(evt).await;
                                outcome = DriverOutcome::Completed {
                                    metadata: last_metadata.clone(),
                                    citations: std::mem::take(&mut text_citations),
                                };
                                break;
                            }
                            Ok(StreamingEvent::Error(e)) => {
                                // Phase 7 overflow-detection hook. The
                                // plugin signals context-window exhaustion
                                // via the `context_overflow:` prefix (per
                                // ADR-0023). When observed, record the
                                // event for the overflow-recovery path —
                                // the actual `on_session_summary` retry
                                // lives in Phase 8.
                                if is_context_overflow_error(&e.error) {
                                    overflow_observed = true;
                                    warn!(
                                        assistant_message_id = %assistant_id,
                                        error = %e.error,
                                        "plugin emitted context_overflow streaming error",
                                    );
                                }
                                let evt = StreamingEvent::Error(StreamingErrorEvent {
                                    message_id: assistant_id,
                                    error: e.error.clone(),
                                });
                                emitter.emit(evt).await;
                                outcome = DriverOutcome::Errored {
                                    error: e.error,
                                    finish_reason: "error",
                                };
                                break;
                            }
                            Err(err) => {
                                let error_str = err.to_string();
                                let finish_reason = finish_reason_for(&err);
                                let evt = StreamingEvent::Error(StreamingErrorEvent {
                                    message_id: assistant_id,
                                    error: error_str.clone(),
                                });
                                emitter.emit(evt).await;
                                outcome = DriverOutcome::Errored {
                                    error: error_str,
                                    finish_reason,
                                };
                                break;
                            }
                        }
                    }
                }
            }

            // Persist the final state. Errors here are logged but never
            // propagated — the wire stream is already closed and a
            // database hiccup must not cause the connection to hang.
            let persist = match outcome {
                DriverOutcome::Completed {
                    metadata,
                    citations,
                } => {
                    // Fold accumulated `State`/`Tool` events into the message
                    // metadata under `state` / `tools` (FR-024 Phase B).
                    let metadata = merge_stream_metadata(metadata, state_patch, tool_traces);
                    messages
                        .finalize_assistant(
                            session_id,
                            assistant_id,
                            FinalizeOutcome::Complete {
                                text: accumulator,
                                metadata,
                                citations,
                                extra_parts,
                            },
                        )
                        .await
                }
                DriverOutcome::CancelledByClient => {
                    messages
                        .finalize_assistant(
                            session_id,
                            assistant_id,
                            FinalizeOutcome::Cancelled { text: accumulator },
                        )
                        .await
                }
                DriverOutcome::Errored {
                    error,
                    finish_reason,
                } => {
                    messages
                        .finalize_assistant(
                            session_id,
                            assistant_id,
                            FinalizeOutcome::Errored {
                                text: accumulator,
                                error,
                                finish_reason,
                            },
                        )
                        .await
                }
            };

            if let Err(err) = persist {
                warn!(
                    assistant_message_id = %assistant_id,
                    error = %err,
                    "failed to finalize assistant message after stream end"
                );
            }

            // FR-024 Phase B: apply the merged `SessionMeta` patch to the owning
            // session (shallow read-modify-write). Best-effort — a failure is
            // logged and never blocks the (already closed) stream.
            if !session_patch.is_empty()
                && let Some((sessions, tenant, user, sid)) = session_persist
            {
                apply_session_meta_patch(sessions.as_ref(), &tenant, &user, sid, session_patch)
                    .await;
            }

            // Phase 7: dispatch the overflow hook AFTER the stream has been
            // finalised so the assistant stub state is consistent. Errors
            // from the hook are logged but never propagated — the wire
            // stream has already closed and Phase 8 will own the retry.
            if overflow_observed && let Some(ctx) = overflow_ctx {
                let svc = ctx.service.clone();
                let sessions = ctx.sessions.clone();
                let session_id = ctx.session_id;
                let identity_tenant = ctx.tenant_id.clone();
                let identity_user = ctx.user_id.clone();
                tokio::spawn(async move {
                    match sessions
                        .find_by_id(&identity_tenant, &identity_user, session_id)
                        .await
                    {
                        Ok(Some(row)) => {
                            let meta = row.metadata.clone().unwrap_or(JsonValue::Null);
                            let strategy = read_memory_strategy(&meta);
                            let res = svc
                                .handle_context_overflow(
                                    &identity_tenant,
                                    &identity_user,
                                    session_id,
                                    &strategy,
                                )
                                .await;
                            if let Err(err) = res {
                                debug!(
                                    session_id = %session_id,
                                    strategy_type = %strategy_type_label(&strategy),
                                    error = %err,
                                    "context_overflow hook returned (Phase 7 dispatch \u{2014} Phase 8 owns retry)",
                                );
                            }
                        }
                        Ok(None) => {
                            // Session disappeared between dispatch and the
                            // hook fire — nothing to do.
                            debug!(
                                session_id = %session_id,
                                "context_overflow hook: session no longer accessible",
                            );
                        }
                        Err(err) => {
                            warn!(
                                session_id = %session_id,
                                error = %err,
                                "context_overflow hook: failed to load session row",
                            );
                        }
                    }
                });
            }
        });

        // Bridge `mpsc::Receiver<StreamingEvent>` to a `BoxStream` without
        // pulling `tokio-stream` into our Cargo manifest (workspace
        // wiring is owned by Phase 15).
        stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|evt| (evt, rx)) },
        )
        .boxed()
    }
}

/// Phase 7 — driver-side overflow-dispatch wiring.
///
/// Captured eagerly by [`MessageService::send_message`] /
/// [`MessageService::dispatch_to_plugin`] before they spawn the driver
/// task. The driver fires the hook once, after the stream has been
/// finalised, only when the plugin emitted a `context_overflow:`
/// streaming error.
///
/// The struct holds `Arc`s so the spawned task can take ownership
/// without borrowing anything off the request scope.
#[domain_model]
#[derive(Clone)]
struct OverflowDispatchCtx {
    /// Cloned `MessageService` — used to call `handle_context_overflow`.
    service: MessageService,
    /// Session repo — needed to re-read the (possibly just-updated)
    /// memory strategy from `session.metadata` before dispatching.
    sessions: Arc<dyn SessionRepo>,
    /// Owning session.
    session_id: Uuid,
    /// JWT-derived tenant id of the calling identity (scopes the
    /// session lookup).
    tenant_id: String,
    /// JWT-derived user id of the calling identity.
    user_id: String,
}

/// Fold accumulated `State` / `Tool` stream events into the message metadata
/// under `state` / `tools` (FR-024 Phase B), preserving the plugin's existing
/// metadata keys.
fn merge_stream_metadata(
    base: Option<JsonValue>,
    state: Option<JsonValue>,
    tools: Vec<JsonValue>,
) -> Option<JsonValue> {
    if state.is_none() && tools.is_empty() {
        return base;
    }
    let mut map = match base {
        Some(JsonValue::Object(m)) => m,
        Some(other) => {
            // Preserve non-object metadata under `_meta` so state/tools can sit
            // at the top level.
            let mut m = serde_json::Map::new();
            m.insert("_meta".to_owned(), other);
            m
        }
        None => serde_json::Map::new(),
    };
    if let Some(s) = state {
        map.insert("state".to_owned(), s);
    }
    if !tools.is_empty() {
        map.insert("tools".to_owned(), JsonValue::Array(tools));
    }
    Some(JsonValue::Object(map))
}

/// Shallow read-modify-write of a session's `metadata` with `patch` (FR-024
/// Phase B). Best-effort: logs and returns on any failure so a streamed
/// `SessionMeta` event never blocks the (already closed) message stream.
async fn apply_session_meta_patch(
    sessions: &dyn SessionRepo,
    tenant_id: &str,
    user_id: &str,
    session_id: Uuid,
    patch: serde_json::Map<String, JsonValue>,
) {
    let current = match sessions.find_by_id(tenant_id, user_id, session_id).await {
        Ok(Some(row)) => row.metadata,
        Ok(None) => return,
        Err(err) => {
            warn!(session_id = %session_id, error = %err,
                "session.meta patch: failed to load session; skipping");
            return;
        }
    };
    let mut map = match current {
        Some(JsonValue::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in patch {
        map.insert(k, v);
    }
    if let Err(err) = sessions
        .update_metadata(tenant_id, user_id, session_id, Some(JsonValue::Object(map)))
        .await
    {
        warn!(session_id = %session_id, error = %err,
            "session.meta patch: update_metadata failed");
    }
}

/// Internal state machine result of the driver loop. Bridges the
/// streaming `select!` arm exits back to the matching
/// [`FinalizeOutcome`] for persistence.
#[domain_model]
#[derive(Debug, Clone)]
enum DriverOutcome {
    Completed {
        metadata: Option<JsonValue>,
        citations: PartCitations,
    },
    CancelledByClient,
    Errored {
        error: String,
        finish_reason: &'static str,
    },
}

/// Subset of session-derived fields the streaming pipeline needs after
/// validation has succeeded.
#[domain_model]
#[derive(Debug, Clone)]
struct ValidatedRequest {
    session_type_id: Uuid,
    plugin_instance_id: String,
}

/// Reject client-supplied `Message.metadata` that writes an engine-reserved
/// key or exceeds [`MAX_MESSAGE_METADATA_BYTES`] once serialized. The
/// message-side counterpart of
/// [`crate::domain::service::session_service::reject_reserved_metadata`]
/// (ADR-0017), with the size cap added because the blob is persisted for the
/// session's whole retention window.
//
// @cpt-cf-chat-engine-algo-message-processing-validate-request
pub fn validate_message_metadata(metadata: Option<&JsonValue>) -> Result<()> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    if let JsonValue::Object(map) = metadata {
        for key in RESERVED_MESSAGE_METADATA_KEYS {
            if map.contains_key(*key) {
                return Err(ChatEngineError::bad_request(format!(
                    "metadata key '{key}' is reserved and cannot be set by clients"
                )));
            }
        }
    }
    let size = serde_json::to_vec(metadata)
        .map_err(|e| ChatEngineError::bad_request(format!("metadata is not serializable: {e}")))?
        .len();
    if size > MAX_MESSAGE_METADATA_BYTES {
        return Err(ChatEngineError::bad_request(format!(
            "metadata is {size} bytes serialized, exceeding the {MAX_MESSAGE_METADATA_BYTES}-byte limit"
        )));
    }
    Ok(())
}

/// Names of capabilities currently enabled on a session, decoded from the
/// session row's `enabled_capabilities` JSONB. Returns an empty vector if
/// the column is absent or shape is unexpected.
fn capability_names_from_session(value: Option<&JsonValue>) -> Vec<String> {
    let Some(JsonValue::Array(arr)) = value else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| match entry {
            JsonValue::Object(map) => map.get("name").and_then(|n| n.as_str()).map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// Canonical short label for a [`MemoryStrategy`] discriminant. Used by
/// observability hooks (logs, metrics) so dashboards don't have to parse
/// the full enum payload.
fn strategy_type_label(s: &MemoryStrategy) -> &'static str {
    match s {
        MemoryStrategy::Full => "full",
        MemoryStrategy::SlidingWindow { .. } => "sliding_window",
        MemoryStrategy::Summarized { .. } => "summarized",
    }
}

/// Extract an optional list of summarized message ids from the plugin's
/// `Complete` metadata. Mirrors `IntelligenceService`'s helper — the SDK
/// convention places this under
/// `metadata.summarized_message_ids: [uuid, ...]`. Malformed shapes
/// collapse to an empty list so a plugin that omits the field does not
/// break the recovery flow.
fn extract_summarized_ids_from_meta(meta: &JsonValue) -> Vec<Uuid> {
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

/// Map a [`PluginError`] to the canonical `finish_reason` label persisted
/// in `metadata` when the stream fails.
fn finish_reason_for(err: &PluginError) -> &'static str {
    match err {
        PluginError::Timeout { .. } => "timeout",
        PluginError::Transient { .. } | PluginError::RateLimited { .. } => "interrupted",
        _ => "error",
    }
}

#[cfg(test)]
#[path = "message_service_tests.rs"]
mod message_service_tests;
