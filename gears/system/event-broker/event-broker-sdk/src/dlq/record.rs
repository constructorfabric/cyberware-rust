use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::consumer::RawEvent;
use crate::error::ConsumerError;
use crate::ids::{ConsumerGroupId, TopicId};
use crate::sequence::Sequence;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterRecord {
    pub group_id: Option<ConsumerGroupId>,
    pub topic_id: Option<TopicId>,
    pub topic: String,
    pub event_type: String,
    pub subject: String,
    pub subject_type: String,
    pub partition: u32,
    pub offset: Sequence,
    pub attempts: Option<u16>,
    pub reason: String,
    pub payload: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
    pub event_id: Uuid,
}

impl DeadLetterRecord {
    pub fn builder(event: &RawEvent, reason: impl Into<String>) -> DeadLetterRecordBuilder {
        DeadLetterRecordBuilder {
            event: event.clone(),
            reason: reason.into(),
            group_id: None,
            topic_id: Some(TopicId::from_gts(&event.topic)),
            attempts: None,
            occurred_at: Utc::now(),
        }
    }

    pub fn from_event(event: &RawEvent, reason: impl Into<String>) -> Self {
        Self::builder(event, reason).build()
    }
}

pub struct DeadLetterRecordBuilder {
    event: RawEvent,
    reason: String,
    group_id: Option<ConsumerGroupId>,
    topic_id: Option<TopicId>,
    attempts: Option<u16>,
    occurred_at: DateTime<Utc>,
}

impl DeadLetterRecordBuilder {
    pub fn group_id(mut self, group_id: ConsumerGroupId) -> Self {
        self.group_id = Some(group_id);
        self
    }

    pub fn topic_id(mut self, topic_id: TopicId) -> Self {
        self.topic_id = Some(topic_id);
        self
    }

    pub fn without_topic_id(mut self) -> Self {
        self.topic_id = None;
        self
    }

    pub fn attempts(mut self, attempts: u16) -> Self {
        self.attempts = Some(attempts);
        self
    }

    pub fn occurred_at(mut self, occurred_at: DateTime<Utc>) -> Self {
        self.occurred_at = occurred_at;
        self
    }

    pub fn build(self) -> DeadLetterRecord {
        DeadLetterRecord {
            group_id: self.group_id,
            topic_id: self.topic_id,
            topic: self.event.topic.clone(),
            event_type: self.event.type_id.clone(),
            subject: self.event.subject.clone(),
            subject_type: self.event.subject_type.clone(),
            partition: self.event.partition,
            offset: self.event.offset,
            attempts: self.attempts,
            reason: self.reason,
            payload: self.event.data.clone(),
            occurred_at: self.occurred_at,
            event_id: self.event.id,
        }
    }
}

#[async_trait::async_trait]
pub trait DeadLetterSink: Send + Sync {
    async fn park(&self, record: DeadLetterRecord) -> Result<(), ConsumerError>;
}
