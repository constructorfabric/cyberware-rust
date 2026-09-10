use std::sync::Arc;

use crate::error::{ConsumerError, EventBrokerError};

use super::{DeadLetterEnvelope, DeadLetterRecord};

#[derive(Clone)]
pub struct ConsumerDlqOutbox {
    outbox: Arc<toolkit_db::outbox::Outbox>,
    queue: String,
    partitions: u32,
}

pub struct ConsumerDlqOutboxBuilder {
    outbox: Arc<toolkit_db::outbox::Outbox>,
    queue: Option<String>,
    partitions: Option<u32>,
}

impl ConsumerDlqOutbox {
    pub fn builder(outbox: Arc<toolkit_db::outbox::Outbox>) -> ConsumerDlqOutboxBuilder {
        ConsumerDlqOutboxBuilder {
            outbox,
            queue: None,
            partitions: None,
        }
    }

    pub fn queue(&self) -> &str {
        &self.queue
    }

    pub fn partitions(&self) -> u32 {
        self.partitions
    }

    pub fn partition_for_record(&self, record: &DeadLetterRecord) -> u32 {
        record.partition % self.partitions
    }

    pub async fn enqueue(
        &self,
        runner: &(impl toolkit_db::secure::DBRunner + Sync + ?Sized),
        record: DeadLetterRecord,
    ) -> Result<toolkit_db::outbox::OutboxMessageId, ConsumerError> {
        let partition = self.partition_for_record(&record);
        let envelope = DeadLetterEnvelope::from_record(record);
        let payload = envelope.to_vec()?;

        let message = toolkit_db::outbox::Record::to(&self.queue, partition)
            .payload(payload, DeadLetterEnvelope::PAYLOAD_TYPE)
            .build()
            .map_err(|err| {
                EventBrokerError::Internal(format!("enqueue dead-letter envelope: {err}"))
            })?;
        self.outbox.enqueue(runner, message).await.map_err(|err| {
            EventBrokerError::Internal(format!("enqueue dead-letter envelope: {err}"))
        })
    }
}

impl ConsumerDlqOutboxBuilder {
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    pub fn partitions(mut self, partitions: u32) -> Self {
        self.partitions = Some(partitions);
        self
    }

    pub fn build(self) -> ConsumerDlqOutbox {
        ConsumerDlqOutbox {
            outbox: self.outbox,
            queue: self.queue.unwrap_or_else(|| "consumer-dlq".to_owned()),
            partitions: self.partitions.unwrap_or(1).max(1),
        }
    }
}
