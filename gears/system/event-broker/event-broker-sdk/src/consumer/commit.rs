#[cfg(feature = "db")]
use crate::error::ConsumerError;
#[cfg(feature = "db")]
use crate::sequence::Sequence;

/// Commit handle for tx-capable consumers (`db` feature).
/// Offers `commit_offset_in_tx`, which writes a delivered offset into the
/// caller's transaction atomically with the handler's business writes.
///
/// Generic over `OM: CommitOffsetInTx` so `commit_offset_in_tx` can call
/// `OM::commit_in_tx` without boxing. Handler impls use the concrete OM type
/// (`TxCommitHandle<LocalDbOffsetManager>`).
#[cfg(feature = "db")]
pub struct TxCommitHandle<OM: super::CommitOffsetInTx> {
    pub(crate) partition: u32,
    pub(crate) batch_offsets: Vec<Sequence>,
    pub(crate) offset_manager: std::sync::Arc<OM>,
    pub(crate) group: crate::ids::ConsumerGroupId,
    pub(crate) topic: crate::ids::TopicId,
    /// Offset successfully written inside the user transaction.
    pub(crate) committed_offset: std::sync::Arc<std::sync::Mutex<Option<Sequence>>>,
}

#[cfg(feature = "db")]
pub(crate) struct TxCommitHandleParts<OM: super::CommitOffsetInTx> {
    pub(crate) partition: u32,
    pub(crate) offsets: Vec<Sequence>,
    pub(crate) offset_manager: std::sync::Arc<OM>,
    pub(crate) group: crate::ids::ConsumerGroupId,
    pub(crate) topic: crate::ids::TopicId,
}

#[cfg(feature = "db")]
impl<OM: super::CommitOffsetInTx> TxCommitHandle<OM> {
    pub(crate) fn new(parts: TxCommitHandleParts<OM>) -> Self {
        Self {
            partition: parts.partition,
            batch_offsets: parts.offsets,
            offset_manager: parts.offset_manager,
            group: parts.group,
            topic: parts.topic,
            committed_offset: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub async fn commit_offset_in_tx<TX>(
        &self,
        txn: &TX,
        offset: Sequence,
    ) -> Result<(), ConsumerError>
    where
        TX: toolkit_db::secure::DBRunner + Sync,
    {
        if !self.batch_offsets.contains(&offset) {
            return Err(crate::error::EventBrokerError::InvalidConsumerOptions {
                detail: format!(
                    "committed offset {offset} is not present in delivered batch offsets {:?}",
                    self.batch_offsets
                ),
                instance: String::new(),
            });
        }

        self.offset_manager
            .commit_in_tx(txn, &self.group, &self.topic, self.partition, offset)
            .await
            .map_err(crate::error::EventBrokerError::OffsetManager)?;
        *self.committed_offset.lock().map_err(|_| {
            crate::error::EventBrokerError::Internal("tx commit state mutex poisoned".into())
        })? = Some(offset);
        Ok(())
    }
}
