use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ConsumerError;
use crate::ids::{ConsumerGroupId, TopicId};

use super::DeadLetterRecord;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeadLetterEnvelope {
    pub version: u16,
    pub group_id: Option<ConsumerGroupId>,
    pub topic_id: Option<TopicId>,
    pub topic: String,
    pub event_type: String,
    pub subject: String,
    pub subject_type: String,
    pub partition: u32,
    pub offset: i64,
    pub attempts: Option<u16>,
    pub reason: String,
    pub payload: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub parked_at: DateTime<Utc>,
    pub event_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeadLetterSourceCoordinates {
    pub group_id: Option<ConsumerGroupId>,
    pub topic_id: Option<TopicId>,
    pub topic: String,
    pub event_type: String,
    pub subject: String,
    pub subject_type: String,
    pub partition: u32,
    pub offset: i64,
    pub event_id: Uuid,
}

impl DeadLetterEnvelope {
    /// Bumped only when a stored envelope's shape changes in a way a reader must
    /// detect. The version is a compatibility marker for envelopes already in a
    /// DLQ, so a change made before anything ships leaves it alone: there are no
    /// stored envelopes of an older shape for a reader to distinguish.
    pub const VERSION: u16 = 1;
    pub const PAYLOAD_TYPE: &'static str =
        "application/vnd.constructorfabric.event-broker.dlq+json";

    pub fn from_record(record: DeadLetterRecord) -> Self {
        Self {
            version: Self::VERSION,
            group_id: record.group_id,
            topic_id: record.topic_id,
            topic: record.topic,
            event_type: record.event_type,
            subject: record.subject,
            subject_type: record.subject_type,
            partition: record.partition,
            offset: record.offset,
            attempts: record.attempts,
            reason: record.reason,
            payload: record.payload,
            occurred_at: record.occurred_at,
            parked_at: Utc::now(),
            event_id: record.event_id,
        }
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, ConsumerError> {
        serde_json::to_vec(self).map_err(|err| {
            crate::error::EventBrokerError::Internal(format!(
                "serialize dead-letter envelope: {err}"
            ))
        })
    }

    pub fn from_slice(payload: &[u8]) -> Result<Self, ConsumerError> {
        serde_json::from_slice(payload).map_err(|err| {
            crate::error::EventBrokerError::Internal(format!(
                "deserialize dead-letter envelope: {err}"
            ))
        })
    }

    pub fn source_coordinates(&self) -> DeadLetterSourceCoordinates {
        DeadLetterSourceCoordinates {
            group_id: self.group_id,
            topic_id: self.topic_id,
            topic: self.topic.clone(),
            event_type: self.event_type.clone(),
            subject: self.subject.clone(),
            subject_type: self.subject_type.clone(),
            partition: self.partition,
            offset: self.offset,
            event_id: self.event_id,
        }
    }
}
