use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    MiniChatAuditPluginError, PublishError, TurnMutationAuditEventType, UsageEvent,
};
use toolkit_db::outbox::Outbox;
use tracing::{info, warn};

use crate::domain::error::DomainError;
use crate::domain::model::audit_envelope::AuditEnvelope;
use crate::domain::ports::{MiniChatMetricsPort, metric_labels};
use crate::domain::repos::{AttachmentCleanupEvent, ChatCleanupEvent, OutboxEnqueuer};
use crate::infra::audit_gateway::AuditGateway;

const AUDIT_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Infrastructure implementation of [`OutboxEnqueuer`].
///
/// Serializes events to JSON and inserts into the outbox table
/// within the caller's transaction via `toolkit_db::outbox::Outbox::enqueue()`.
///
/// The `Outbox` handle is set lazily via [`set_outbox`] — this allows the
/// enqueuer to be constructed in `init()` (where services need it) while the
/// outbox pipeline starts later in `start()` (after OAGW registration).
/// Enqueue is never called before `start()` because HTTP traffic doesn't arrive
/// until after all gears have started.
pub struct InfraOutboxEnqueuer {
    outbox: std::sync::OnceLock<Arc<Outbox>>,
    usage_queue_name: String,
    cleanup_queue_name: String,
    chat_cleanup_queue_name: String,
    #[allow(dead_code)]
    thread_summary_queue_name: String,
    audit_queue_name: String,
    num_partitions: u32,
}

/// Build one outbox message for this gear.
///
/// Every queue here carries JSON, so the payload type is stated once rather
/// than at each of the six call sites, and a rejected request becomes a domain
/// error before any statement runs.
fn outbox_message(
    queue: &str,
    partition: u32,
    payload: Vec<u8>,
) -> Result<toolkit_db::outbox::Record<'_>, DomainError> {
    toolkit_db::outbox::Record::to(queue, partition)
        .payload(payload, "application/json")
        .build()
        .map_err(|e| DomainError::internal(format!("outbox request: {e}")))
}

impl InfraOutboxEnqueuer {
    pub(crate) fn new(
        usage_queue_name: String,
        cleanup_queue_name: String,
        chat_cleanup_queue_name: String,
        thread_summary_queue_name: String,
        audit_queue_name: String,
        num_partitions: u32,
    ) -> Self {
        Self {
            outbox: std::sync::OnceLock::new(),
            usage_queue_name,
            cleanup_queue_name,
            chat_cleanup_queue_name,
            thread_summary_queue_name,
            audit_queue_name,
            num_partitions,
        }
    }

    /// Set the outbox handle after the pipeline starts in `start()`.
    /// Panics if called more than once.
    pub(crate) fn set_outbox(&self, outbox: Arc<Outbox>) {
        assert!(
            self.outbox.set(outbox).is_ok(),
            "InfraOutboxEnqueuer::set_outbox called twice"
        );
    }

    fn outbox(&self) -> &Outbox {
        #[allow(clippy::expect_used)]
        self.outbox
            .get()
            .expect("outbox not set -- enqueue called before start()")
    }

    fn partition_for(&self, tenant_id: uuid::Uuid) -> u32 {
        Self::compute_partition(tenant_id, self.num_partitions)
    }

    fn compute_partition(tenant_id: uuid::Uuid, num_partitions: u32) -> u32 {
        let hash = tenant_id.as_u128();
        #[allow(clippy::cast_possible_truncation)]
        {
            (hash % u128::from(num_partitions)) as u32
        }
    }

    /// Enqueue a thread summary task event within the caller's transaction.
    ///
    /// Partitions by `chat_id` so all summary events for a given chat land in
    /// the same partition (processed in order by a single consumer).
    #[allow(dead_code)]
    pub async fn enqueue_thread_summary_task(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        chat_id: uuid::Uuid,
        payload: Vec<u8>,
    ) -> Result<(), DomainError> {
        let partition = Self::compute_partition(chat_id, self.num_partitions);

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.thread_summary_queue_name, partition, payload)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.thread_summary_queue_name,
            partition,
            chat_id = %chat_id,
            "thread summary task enqueued"
        );

        Ok(())
    }
}

#[async_trait]
impl OutboxEnqueuer for InfraOutboxEnqueuer {
    async fn enqueue_usage_event(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        event: UsageEvent,
    ) -> Result<(), DomainError> {
        let partition = self.partition_for(event.tenant_id);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize UsageEvent: {e}")))?;

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.usage_queue_name, partition, payload)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.usage_queue_name,
            partition,
            tenant_id = %event.tenant_id,
            turn_id = ?event.turn_id,
            "usage event enqueued"
        );

        Ok(())
    }

    async fn enqueue_attachment_cleanup(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        event: AttachmentCleanupEvent,
    ) -> Result<(), DomainError> {
        let partition = self.partition_for(event.tenant_id);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize AttachmentCleanupEvent: {e}")))?;

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.cleanup_queue_name, partition, payload)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.cleanup_queue_name,
            partition,
            tenant_id = %event.tenant_id,
            attachment_id = %event.attachment_id,
            "attachment cleanup event enqueued"
        );

        Ok(())
    }

    async fn enqueue_chat_cleanup(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        event: ChatCleanupEvent,
    ) -> Result<(), DomainError> {
        // Partition by chat_id so all cleanup messages for the same chat
        // are serialized within one partition.
        let partition = Self::compute_partition(event.chat_id, self.num_partitions);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize ChatCleanupEvent: {e}")))?;

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.chat_cleanup_queue_name, partition, payload)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.chat_cleanup_queue_name,
            partition,
            chat_id = %event.chat_id,
            system_request_id = %event.system_request_id,
            "chat cleanup event enqueued"
        );

        Ok(())
    }

    async fn enqueue_audit_event(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        event: AuditEnvelope,
    ) -> Result<(), DomainError> {
        let tenant_id = match &event {
            AuditEnvelope::Turn(e) => e.tenant_id,
            AuditEnvelope::Mutation(e) => e.tenant_id,
            AuditEnvelope::Delete(e) => e.tenant_id,
        };
        let partition = self.partition_for(tenant_id);
        let payload = serde_json::to_vec(&event)
            .map_err(|e| DomainError::internal(format!("serialize AuditEnvelope: {e}")))?;

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.audit_queue_name, partition, payload)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("audit outbox enqueue: {e}")))?;

        info!(
        queue = %self.audit_queue_name,
        partition,
        %tenant_id,
        "audit event enqueued"
        );

        Ok(())
    }

    async fn enqueue_thread_summary(
        &self,
        runner: &(dyn toolkit_db::secure::DBRunner + Sync),
        payload: crate::domain::repos::ThreadSummaryTaskPayload,
    ) -> Result<(), DomainError> {
        let partition = Self::compute_partition(payload.chat_id, self.num_partitions);
        let serialized = serde_json::to_vec(&payload).map_err(|e| {
            DomainError::internal(format!("serialize ThreadSummaryTaskPayload: {e}"))
        })?;

        self.outbox()
            .enqueue(
                runner,
                outbox_message(&self.thread_summary_queue_name, partition, serialized)?,
            )
            .await
            .map_err(|e| DomainError::internal(format!("outbox enqueue: {e}")))?;

        info!(
            queue = %self.thread_summary_queue_name,
            partition,
            chat_id = %payload.chat_id,
            system_request_id = %payload.system_request_id,
            "thread summary task enqueued"
        );

        Ok(())
    }

    fn flush(&self) {
        // flush is a no-op if outbox isn't set yet (before start).
        if let Some(outbox) = self.outbox.get() {
            outbox.flush();
        }
    }
}

/// Trait for lazily resolving the model-policy plugin.
///
/// Production code uses `ModelPolicyGateway` (lazy GTS resolution).
/// Tests provide a direct `Arc<dyn MiniChatModelPolicyPluginClientV1>`.
#[async_trait]
pub trait PolicyPluginProvider: Send + Sync {
    async fn get_plugin(
        &self,
    ) -> Result<
        Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>,
        crate::domain::error::DomainError,
    >;
}

#[async_trait]
impl PolicyPluginProvider for crate::infra::model_policy::ModelPolicyGateway {
    async fn get_plugin(
        &self,
    ) -> Result<
        Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>,
        crate::domain::error::DomainError,
    > {
        self.get_policy_plugin().await
    }
}

/// Delivers usage events to the model-policy plugin via `publish_usage()`.
///
/// Deserializes `OutboxMessage.payload` into `UsageEvent`, resolves the plugin
/// lazily via [`PolicyPluginProvider`], calls `publish_usage()`, and maps
/// `PublishError` variants to outbox `MessageResult`:
/// - `Ok(())` → `Ok` (ack + advance cursor)
/// - `PublishError::Transient` → `Retry` (exponential backoff, redelivery)
/// - `PublishError::Permanent` → `Reject` (dead-letter for manual inspection)
/// - Deserialization failure → `Reject` (corrupt payload, permanent)
/// - Plugin resolution failure → `Retry` (transient - plugin may not be ready yet)
pub struct UsageEventHandler {
    pub(crate) plugin_provider: Arc<dyn PolicyPluginProvider>,
}

#[async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for UsageEventHandler {
    #[tracing::instrument(name = "worker", skip_all, fields(worker = "usage_event"))]
    async fn handle(
        &self,
        msg: &toolkit_db::outbox::OutboxMessage,
    ) -> toolkit_db::outbox::MessageResult {
        let event = match serde_json::from_slice::<UsageEvent>(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    payload_len = msg.payload.len(),
                    "usage event deserialization failed: {e}"
                );
                return toolkit_db::outbox::MessageResult::Reject(format!(
                    "deserialization failed: {e}"
                ));
            }
        };

        info!(
            tenant_id = %event.tenant_id,
            user_id = ?event.user_id,
            turn_id = ?event.turn_id,
            request_id = %event.request_id,
            effective_model = %event.effective_model,
            billing_outcome = ?event.billing_outcome,
            settlement_method = ?event.settlement_method,
            actual_credits_micro = event.actual_credits_micro,
            partition_id = msg.partition_id,
            seq = msg.seq,
            "publishing usage event to plugin"
        );

        let plugin = match self.plugin_provider.get_plugin().await {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "failed to resolve policy plugin - will retry"
                );
                return toolkit_db::outbox::MessageResult::Retry;
            }
        };

        match plugin.publish_usage(event).await {
            Ok(()) => toolkit_db::outbox::MessageResult::Ok,
            Err(PublishError::Transient(reason)) => {
                warn!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    %reason,
                    "publish_usage transient failure - will retry"
                );
                toolkit_db::outbox::MessageResult::Retry
            }
            Err(PublishError::Permanent(reason)) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    %reason,
                    "publish_usage permanent failure - dead-lettering"
                );
                toolkit_db::outbox::MessageResult::Reject(reason)
            }
        }
    }
}

/// Delivers audit events to the audit plugin via [`AuditGateway`].
///
/// Deserializes `OutboxMessage.payload` into [`AuditEnvelope`], resolves the
/// plugin via `AuditGateway`, dispatches to the correct `emit_*` method, and
/// maps [`MiniChatAuditPluginError`] to outbox `MessageResult`:
/// - `Ok(())` → `Ok`
/// - `Transient` → `Retry`
/// - `Permanent` → `Reject` (dead-letter)
/// - Deserialization failure → `Reject` (corrupt payload)
/// - Plugin not configured → `Ok` (audit is optional; skip silently)
/// - Plugin resolution error → `Retry` (transient; plugin may not be ready yet)
pub struct AuditEventHandler {
    pub(crate) audit_gateway: Arc<AuditGateway>,
    pub(crate) metrics: Arc<dyn MiniChatMetricsPort>,
}

#[async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for AuditEventHandler {
    #[tracing::instrument(name = "worker", skip_all, fields(worker = "audit_event"))]
    async fn handle(
        &self,
        msg: &toolkit_db::outbox::OutboxMessage,
    ) -> toolkit_db::outbox::MessageResult {
        let plugin = match self.audit_gateway.get_plugin().await {
            Ok(Some(p)) => p,
            Ok(None) => {
                // No audit plugin registered - audit is optional; ack and advance.
                return toolkit_db::outbox::MessageResult::Ok;
            }
            Err(e) => {
                warn!(error = %e, "audit plugin resolution failed - will retry");
                return toolkit_db::outbox::MessageResult::Retry;
            }
        };

        let envelope = match serde_json::from_slice::<AuditEnvelope>(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    payload_len = msg.payload.len(),
                    "audit event deserialization failed: {e}"
                );
                return toolkit_db::outbox::MessageResult::Reject(format!(
                    "deserialization failed: {e}"
                ));
            }
        };

        let result: Result<(), MiniChatAuditPluginError> = match &envelope {
            AuditEnvelope::Turn(evt) => {
                tokio::time::timeout(AUDIT_PLUGIN_TIMEOUT, plugin.emit_turn_audit(evt.clone()))
                    .await
                    .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout))
            }
            AuditEnvelope::Mutation(evt) => match evt.event_type {
                TurnMutationAuditEventType::TurnRetry => tokio::time::timeout(
                    AUDIT_PLUGIN_TIMEOUT,
                    plugin.emit_turn_retry_audit(evt.clone()),
                )
                .await
                .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout)),
                TurnMutationAuditEventType::TurnEdit => tokio::time::timeout(
                    AUDIT_PLUGIN_TIMEOUT,
                    plugin.emit_turn_edit_audit(evt.clone()),
                )
                .await
                .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout)),
            },
            AuditEnvelope::Delete(evt) => tokio::time::timeout(
                AUDIT_PLUGIN_TIMEOUT,
                plugin.emit_turn_delete_audit(evt.clone()),
            )
            .await
            .unwrap_or(Err(MiniChatAuditPluginError::PluginTimeout)),
        };

        match result {
            Ok(()) => {
                self.metrics.record_audit_emit(metric_labels::result::OK);
                toolkit_db::outbox::MessageResult::Ok
            }
            Err(e) if e.is_transient() => {
                warn!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "audit emit transient failure - will retry"
                );
                self.metrics.record_audit_emit(metric_labels::result::RETRY);
                toolkit_db::outbox::MessageResult::Retry
            }
            Err(e) => {
                tracing::error!(
                    partition_id = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "audit emit permanent failure - dead-lettering"
                );
                self.metrics
                    .record_audit_emit(metric_labels::result::REJECT);
                toolkit_db::outbox::MessageResult::Reject(e.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mini_chat_sdk::{
        MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatModelPolicyPluginClientV1,
        MiniChatModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError,
        TurnAuditEvent, TurnDeleteAuditEvent, TurnEditAuditEvent, TurnRetryAuditEvent, UserLimits,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use time::OffsetDateTime;
    use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
    use uuid::Uuid;

    fn make_usage_event() -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::new_v4(),
            user_id: Some(Uuid::new_v4()),
            chat_id: Uuid::new_v4(),
            turn_id: Some(Uuid::new_v4()),
            request_id: Uuid::new_v4(),
            effective_model: "gpt-4o".to_owned(),
            selected_model: "gpt-4o".to_owned(),
            terminal_state: "completed".to_owned(),
            billing_outcome: "charged".to_owned(),
            usage: None,
            actual_credits_micro: 500,
            settlement_method: "quota".to_owned(),
            policy_version_applied: 1,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: OffsetDateTime::now_utc(),
            requester_type: "user".to_owned(),
            dedupe_key: None,
            system_task_type: None,
        }
    }

    fn make_outbox_message(payload: Vec<u8>) -> OutboxMessage {
        OutboxMessage {
            partition_id: 1,
            seq: 42,
            payload,
            payload_type: "application/json".to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        }
    }

    /// Mock plugin that records `publish_usage` calls and returns a configurable result.
    struct MockPlugin {
        result: std::sync::Mutex<Result<(), PublishError>>,
        call_count: AtomicU32,
        notifier: tokio::sync::Notify,
    }

    impl MockPlugin {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Ok(())),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn transient(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Err(PublishError::Transient(reason.to_owned()))),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn permanent(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Err(PublishError::Permanent(reason.to_owned()))),
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
            })
        }

        fn calls(&self) -> u32 {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl MiniChatModelPolicyPluginClientV1 for MockPlugin {
        async fn get_current_policy_version(
            &self,
            _user_id: Uuid,
        ) -> Result<PolicyVersionInfo, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn get_policy_snapshot(
            &self,
            _user_id: Uuid,
            _policy_version: u64,
        ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn get_user_limits(
            &self,
            _user_id: Uuid,
            _policy_version: u64,
        ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn check_user_license(
            &self,
            _user_id: Uuid,
        ) -> Result<mini_chat_sdk::UserLicenseStatus, MiniChatModelPolicyPluginError> {
            unimplemented!("not needed in outbox tests")
        }

        async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let result = {
                let guard = self.result.lock().unwrap();
                match &*guard {
                    Ok(()) => Ok(()),
                    Err(PublishError::Transient(r)) => Err(PublishError::Transient(r.clone())),
                    Err(PublishError::Permanent(r)) => Err(PublishError::Permanent(r.clone())),
                }
            };
            self.notifier.notify_one();
            result
        }
    }

    /// Wraps a mock plugin as a [`PolicyPluginProvider`] for tests.
    struct MockProvider {
        plugin: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    }

    #[async_trait]
    impl PolicyPluginProvider for MockProvider {
        async fn get_plugin(
            &self,
        ) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, crate::domain::error::DomainError>
        {
            Ok(self.plugin.clone())
        }
    }

    fn make_handler(plugin: &Arc<dyn MiniChatModelPolicyPluginClientV1>) -> UsageEventHandler {
        UsageEventHandler {
            plugin_provider: Arc::new(MockProvider {
                plugin: plugin.clone(),
            }),
        }
    }

    // ── 7.1: partition_for returns values in [0, num_partitions) ──

    #[test]
    fn partition_for_returns_in_range() {
        for num_partitions in [1u32, 2, 4, 8, 16, 32, 64] {
            for _ in 0..100 {
                let tenant_id = Uuid::new_v4();
                let p = InfraOutboxEnqueuer::compute_partition(tenant_id, num_partitions);
                assert!(
                    p < num_partitions,
                    "partition {p} >= num_partitions {num_partitions} for tenant {tenant_id}"
                );
            }
        }
    }

    #[test]
    fn partition_for_deterministic() {
        let tenant_id = Uuid::new_v4();
        let a = InfraOutboxEnqueuer::compute_partition(tenant_id, 4);
        let b = InfraOutboxEnqueuer::compute_partition(tenant_id, 4);
        assert_eq!(a, b);
    }

    // ── 7.2 / 7.7: UsageEventHandler returns Ok when plugin returns Ok ──

    #[tokio::test]
    async fn handler_success_for_valid_event() {
        let plugin = MockPlugin::ok();
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(matches!(result, MessageResult::Ok));
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.3: UsageEventHandler returns Reject for invalid payload ──

    #[tokio::test]
    async fn handler_reject_for_invalid_payload() {
        let plugin = MockPlugin::ok();
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let msg = make_outbox_message(b"not json".to_vec());

        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        match result {
            MessageResult::Reject(reason) => {
                assert!(
                    reason.contains("deserialization failed"),
                    "unexpected reason: {reason}"
                );
            }
            MessageResult::Ok => panic!("expected Reject, got Ok"),
            MessageResult::Retry => panic!("expected Reject, got Retry"),
        }
        // Plugin should not be called for invalid payload.
        assert_eq!(plugin.calls(), 0);
    }

    // ── 7.8: UsageEventHandler returns Retry on PublishError::Transient ──

    #[tokio::test]
    async fn handler_retry_on_transient_error() {
        let plugin = MockPlugin::transient("network timeout");
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(
            matches!(result, MessageResult::Retry),
            "expected Retry, got {result:?}"
        );
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.9: UsageEventHandler returns Reject on PublishError::Permanent ──

    #[tokio::test]
    async fn handler_reject_on_permanent_error() {
        let plugin = MockPlugin::permanent("schema mismatch");
        let handler = make_handler(&(plugin.clone() as Arc<dyn MiniChatModelPolicyPluginClientV1>));
        let event = make_usage_event();
        let payload = serde_json::to_vec(&event).unwrap();
        let msg = make_outbox_message(payload);

        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        match result {
            MessageResult::Reject(reason) => {
                assert_eq!(reason, "schema mismatch");
            }
            MessageResult::Ok => panic!("expected Reject, got Ok"),
            MessageResult::Retry => panic!("expected Reject, got Retry"),
        }
        assert_eq!(plugin.calls(), 1);
    }

    // ── AuditEventHandler unit tests ──

    /// Mock audit plugin that records `emit_*` calls and always returns `Ok(())`.
    enum AuditBehavior {
        Ok,
        Transient(String),
        Permanent(String),
    }

    struct MockAuditPlugin {
        call_count: AtomicU32,
        notifier: tokio::sync::Notify,
        behavior: AuditBehavior,
    }

    impl MockAuditPlugin {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
                behavior: AuditBehavior::Ok,
            })
        }

        fn transient(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
                behavior: AuditBehavior::Transient(msg.to_owned()),
            })
        }

        fn permanent(msg: &str) -> Arc<Self> {
            Arc::new(Self {
                call_count: AtomicU32::new(0),
                notifier: tokio::sync::Notify::new(),
                behavior: AuditBehavior::Permanent(msg.to_owned()),
            })
        }

        fn calls(&self) -> u32 {
            self.call_count.load(Ordering::SeqCst)
        }

        fn record(&self) {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.notifier.notify_one();
        }

        fn emit_result(&self) -> Result<(), MiniChatAuditPluginError> {
            match &self.behavior {
                AuditBehavior::Ok => Ok(()),
                AuditBehavior::Transient(msg) => {
                    Err(MiniChatAuditPluginError::Transient(msg.clone()))
                }
                AuditBehavior::Permanent(msg) => {
                    Err(MiniChatAuditPluginError::Permanent(msg.clone()))
                }
            }
        }
    }

    #[async_trait]
    impl MiniChatAuditPluginClientV1 for MockAuditPlugin {
        async fn emit_turn_audit(&self, _: TurnAuditEvent) -> Result<(), MiniChatAuditPluginError> {
            self.record();
            self.emit_result()
        }
        async fn emit_turn_retry_audit(
            &self,
            _: TurnRetryAuditEvent,
        ) -> Result<(), MiniChatAuditPluginError> {
            self.record();
            self.emit_result()
        }
        async fn emit_turn_edit_audit(
            &self,
            _: TurnEditAuditEvent,
        ) -> Result<(), MiniChatAuditPluginError> {
            self.record();
            self.emit_result()
        }
        async fn emit_turn_delete_audit(
            &self,
            _: TurnDeleteAuditEvent,
        ) -> Result<(), MiniChatAuditPluginError> {
            self.record();
            self.emit_result()
        }
    }

    fn make_audit_envelope_payload() -> Vec<u8> {
        use mini_chat_sdk::{RequesterType, TurnAuditEventType};
        let event = AuditEnvelope::Turn(TurnAuditEvent {
            event_type: TurnAuditEventType::TurnCompleted,
            timestamp: OffsetDateTime::now_utc(),
            tenant_id: Uuid::new_v4(),
            requester_type: RequesterType::User,
            trace_id: None,
            user_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            selected_model: "gpt-4o".to_owned(),
            effective_model: "gpt-4o".to_owned(),
            policy_version_applied: None,
            usage: mini_chat_sdk::AuditUsageTokens {
                input_tokens: 10,
                output_tokens: 20,
                model: None,
                cache_read_input_tokens: None,
                cache_write_input_tokens: None,
                reasoning_tokens: None,
            },
            latency_ms: mini_chat_sdk::LatencyMs::default(),
            policy_decisions: mini_chat_sdk::PolicyDecisions {
                license: None,
                quota: mini_chat_sdk::QuotaDecision {
                    decision: "allowed".to_owned(),
                    quota_scope: None,
                    downgrade_from: None,
                    downgrade_reason: None,
                },
            },
            error_code: None,
            prompt: None,
            response: None,
            attachments: vec![],
            tool_calls: None,
        });
        serde_json::to_vec(&event).unwrap()
    }

    // ── AuditEventHandler: invalid payload → Reject ──
    //
    // Note: the handler only deserializes payloads when a plugin is present.
    // Use an `ok` plugin so the handler reaches the deserialization step.

    #[tokio::test]
    async fn audit_handler_reject_for_invalid_payload() {
        let plugin = MockAuditPlugin::ok();
        let handler = AuditEventHandler {
            audit_gateway: AuditGateway::from_plugin(plugin),
            metrics: Arc::new(crate::domain::ports::metrics::NoopMetrics),
        };
        let msg = make_outbox_message(b"not json".to_vec());
        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(
            matches!(result, MessageResult::Reject(_)),
            "expected Reject for corrupt payload"
        );
    }

    // ── AuditEventHandler: no plugin configured → Ok ──

    #[tokio::test]
    async fn audit_handler_success_when_no_plugin_configured() {
        let handler = AuditEventHandler {
            audit_gateway: AuditGateway::noop(),
            metrics: Arc::new(crate::domain::ports::metrics::NoopMetrics),
        };
        let payload = make_audit_envelope_payload();
        let msg = make_outbox_message(payload);
        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(
            matches!(result, MessageResult::Ok),
            "expected Ok when no plugin configured"
        );
    }

    // ── AuditEventHandler: transient plugin error → Retry ──

    #[tokio::test]
    async fn audit_handler_retry_on_transient_plugin_error() {
        let plugin = MockAuditPlugin::transient("network blip");
        let audit_gateway = AuditGateway::from_plugin(plugin.clone());
        let handler = AuditEventHandler {
            audit_gateway,
            metrics: Arc::new(crate::domain::ports::metrics::NoopMetrics),
        };
        let msg = make_outbox_message(make_audit_envelope_payload());
        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(
            matches!(result, MessageResult::Retry),
            "expected Retry for transient plugin error"
        );
        assert_eq!(plugin.calls(), 1);
    }

    // ── AuditEventHandler: permanent plugin error → Reject ──

    #[tokio::test]
    async fn audit_handler_reject_on_permanent_plugin_error() {
        let plugin = MockAuditPlugin::permanent("schema mismatch");
        let audit_gateway = AuditGateway::from_plugin(plugin.clone());
        let handler = AuditEventHandler {
            audit_gateway,
            metrics: Arc::new(crate::domain::ports::metrics::NoopMetrics),
        };
        let msg = make_outbox_message(make_audit_envelope_payload());
        let result = LeasedMessageHandler::handle(&handler, &msg).await;
        assert!(
            matches!(result, MessageResult::Reject(_)),
            "expected Reject for permanent plugin error"
        );
        assert_eq!(plugin.calls(), 1);
    }

    // ── 7.11: Integration test - AuditEventHandler full pipeline ──

    #[tokio::test]
    async fn audit_pipeline_enqueue_and_deliver() {
        use std::time::Duration;
        use toolkit::client_hub::{ClientHub, ClientScope};
        use toolkit::plugins::GtsPluginSelector;
        use toolkit_db::outbox::{Outbox, Partitions};
        use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

        let plugin = MockAuditPlugin::ok();

        let db = connect_db(
            "sqlite:file:audit_outbox_integration?mode=memory&cache=shared",
            ConnectOpts {
                max_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("connect");

        run_migrations_for_testing(&db, toolkit_db::outbox::outbox_migrations())
            .await
            .expect("outbox migrations");

        // Build an AuditGateway backed by the mock plugin.
        // Pre-warm the selector with the instance ID and register the mock
        // directly in the ClientHub to bypass GTS types-registry resolution.
        let instance_id = "test.audit.plugin.v1~test._.recording.v1";
        let hub = Arc::new(ClientHub::new());
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(
            ClientScope::gts_id(instance_id),
            plugin.clone() as Arc<dyn MiniChatAuditPluginClientV1>,
        );
        let selector = GtsPluginSelector::new();
        selector
            .get_or_init(|| async { Ok::<_, anyhow::Error>(instance_id.to_owned()) })
            .await
            .expect("pre-warm selector");
        let audit_gateway = crate::infra::audit_gateway::AuditGateway::new_preconfigured(
            hub,
            String::new(),
            selector,
        );

        let handle = Outbox::builder(db.clone())
            .queue("test.audit", Partitions::of(1))
            .leased(AuditEventHandler {
                audit_gateway: Arc::clone(&audit_gateway),
                metrics: Arc::new(crate::domain::ports::metrics::NoopMetrics),
            })
            .start()
            .await
            .expect("outbox start");

        let enqueuer = InfraOutboxEnqueuer::new(
            "test.usage".to_owned(),
            "test.cleanup".to_owned(),
            "test.chat_cleanup".to_owned(),
            "test.thread_summary".to_owned(),
            "test.audit".to_owned(),
            1u32,
        );
        enqueuer.set_outbox(Arc::clone(handle.outbox()));

        let payload = make_audit_envelope_payload();
        let envelope: AuditEnvelope = serde_json::from_slice(&payload).unwrap();
        let conn = db.conn().expect("conn");
        enqueuer
            .enqueue_audit_event(&conn, envelope)
            .await
            .expect("enqueue");
        enqueuer.flush();

        tokio::time::timeout(Duration::from_secs(5), plugin.notifier.notified())
            .await
            .expect("audit plugin should have been called within 5s");

        assert_eq!(
            plugin.calls(),
            1,
            "emit_turn_audit should have been called once"
        );

        handle.stop().await;
    }

    // ── 7.5 / 7.10: Integration test - full pipeline with mock plugin ──

    #[tokio::test]
    async fn full_pipeline_enqueue_and_deliver() {
        use std::time::Duration;
        use toolkit_db::outbox::{Outbox, Partitions};
        use toolkit_db::{ConnectOpts, connect_db, migration_runner::run_migrations_for_testing};

        // Mock plugin that tracks calls.
        let plugin = MockPlugin::ok();

        // Set up in-memory DB with outbox migrations.
        let db = connect_db(
            "sqlite:file:outbox_integration?mode=memory&cache=shared",
            ConnectOpts {
                max_conns: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("connect");

        run_migrations_for_testing(&db, toolkit_db::outbox::outbox_migrations())
            .await
            .expect("outbox migrations");

        // Start outbox pipeline with the real UsageEventHandler + mock plugin.
        let handle = Outbox::builder(db.clone())
            .queue("test.usage", Partitions::of(1))
            .leased(UsageEventHandler {
                plugin_provider: Arc::new(MockProvider {
                    plugin: plugin.clone(),
                }),
            })
            .start()
            .await
            .expect("outbox start");

        // Enqueue a usage event using InfraOutboxEnqueuer.
        let enqueuer = InfraOutboxEnqueuer::new(
            "test.usage".to_owned(),
            "test.cleanup".to_owned(),
            "test.chat_cleanup".to_owned(),
            "test.thread_summary".to_owned(),
            "test.audit".to_owned(),
            1u32,
        );
        enqueuer.set_outbox(Arc::clone(handle.outbox()));
        let event = make_usage_event();
        let conn = db.conn().expect("conn");
        enqueuer
            .enqueue_usage_event(&conn, event)
            .await
            .expect("enqueue");
        enqueuer.flush();

        // Wait for the handler to process (notification-based, no fixed sleep).
        tokio::time::timeout(Duration::from_secs(5), plugin.notifier.notified())
            .await
            .expect("plugin should have been called within 5s");

        assert_eq!(
            plugin.calls(),
            1,
            "publish_usage should have been called once"
        );

        handle.stop().await;
    }
}
