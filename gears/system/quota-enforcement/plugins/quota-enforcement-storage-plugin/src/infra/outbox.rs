//! The notification outbox (invariant I11): every mutation enqueues its
//! `NotificationEvent`s in the transaction that writes its rows, through the
//! toolkit outbox under the plugin's own table prefix.
//!
//! The plugin only enqueues. The dispatcher that drains the queue arrives with
//! the notification feature, which registers the queue's handler on the same
//! prefix and binds the handle through [`QeOutbox::bind`]. Until a handle is
//! bound every enqueue fails [`EnqueueError::NotBound`] and the transaction
//! rolls back, so no mutation can commit without its events; that is harmless
//! while the storage client is unpublished and must be wired before it is.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use quota_enforcement_sdk::{NotificationEvent, TenantId};
use toolkit_db::Db;
use toolkit_db::outbox::{EnqueueMessage, Outbox, OutboxError, OutboxHandle, OutboxMessageId};
use toolkit_db::secure::DBRunner;

pub use crate::infra::storage::migrations::OUTBOX_TABLE_PREFIX;

/// The queue every notification event is enqueued on.
pub const NOTIFICATION_QUEUE: &str = "qe_notifications";

/// Partitions of [`NOTIFICATION_QUEUE`]; a tenant always lands on one
/// partition, so its events stay ordered.
pub const NOTIFICATION_PARTITIONS: u16 = 8;

/// Why an enqueue failed inside the caller's transaction.
#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    /// No outbox handle was bound yet.
    #[error("notification outbox is not bound")]
    NotBound,
    /// An event did not serialize.
    #[error("notification event does not serialize: {0}")]
    Serialize(String),
    /// The toolkit outbox rejected the batch.
    #[error(transparent)]
    Outbox(#[from] OutboxError),
}

/// Enqueues notification events on the caller's runner, so they commit with
/// the rows that caused them.
#[async_trait]
pub trait NotificationEnqueuer: Send + Sync {
    /// Enqueue `events` in order. Nothing is written when any event fails.
    async fn enqueue_all(
        &self,
        runner: &(dyn DBRunner + Sync),
        events: &[NotificationEvent],
    ) -> Result<Vec<OutboxMessageId>, EnqueueError>;
}

/// The outbox handle is bound once the pipeline starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("notification outbox is already bound")]
pub struct AlreadyBound;

/// Late-bound handle to the plugin's outbox.
#[derive(Default)]
pub struct QeOutbox {
    outbox: OnceLock<Arc<Outbox>>,
}

impl QeOutbox {
    /// An unbound handle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the started outbox. Once.
    ///
    /// # Errors
    ///
    /// [`AlreadyBound`] on a second call.
    pub fn bind(&self, outbox: Arc<Outbox>) -> Result<(), AlreadyBound> {
        self.outbox.set(outbox).map_err(|_| AlreadyBound)
    }

    /// True once [`Self::bind`] ran.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.outbox.get().is_some()
    }

    /// The partition a tenant's events land on.
    #[must_use]
    pub fn partition_for(tenant_id: TenantId) -> u32 {
        // The remainder of a `u128` by a `u16` fits a `u32`.
        u32::try_from(tenant_id.as_uuid().as_u128() % u128::from(NOTIFICATION_PARTITIONS))
            .unwrap_or(0)
    }
}

#[async_trait]
impl NotificationEnqueuer for QeOutbox {
    async fn enqueue_all(
        &self,
        runner: &(dyn DBRunner + Sync),
        events: &[NotificationEvent],
    ) -> Result<Vec<OutboxMessageId>, EnqueueError> {
        let outbox = self.outbox.get().ok_or(EnqueueError::NotBound)?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let payloads = events
            .iter()
            .map(|event| {
                serde_json::to_vec(event).map_err(|e| EnqueueError::Serialize(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let items: Vec<EnqueueMessage<'_>> = events
            .iter()
            .zip(payloads)
            .map(|(event, payload)| EnqueueMessage {
                partition: Self::partition_for(event.tenant_id),
                payload,
                payload_type: event.kind.as_str(),
            })
            .collect();
        Ok(outbox
            .enqueue_batch(runner, NOTIFICATION_QUEUE, &items)
            .await?)
    }
}

/// Start the outbox pipeline on `db` with the notification queue registered
/// and no handler: enough to enqueue. The dispatcher registers its handler
/// on the same prefix when it lands.
///
/// # Errors
///
/// The toolkit error of the start or the queue registration.
pub async fn start_outbox(db: Db) -> Result<OutboxHandle, OutboxError> {
    let handle = Outbox::builder(db.clone())
        .table_prefix(OUTBOX_TABLE_PREFIX)?
        .start()
        .await?;
    handle
        .outbox()
        .register_queue(&db, NOTIFICATION_QUEUE, NOTIFICATION_PARTITIONS)
        .await?;
    Ok(handle)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "outbox_tests.rs"]
mod outbox_tests;
