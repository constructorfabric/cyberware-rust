//! What a caller hands to the outbox: one entity, or a batch that shares a
//! queue, a payload type and a trace.
//!
//! Both shapes are built rather than constructed as literals, and both validate
//! everything that can be validated without touching the database while they are
//! built - so a rejected submission has issued no statement.
//!
//! Required parts are supplied positionally, in groups whose types cannot be
//! transposed, and each group returns the next stage: a submission with no
//! payload does not compile rather than failing at run time.

use super::validation::{
    validate_payload, validate_payload_type, validate_queue_name, validate_trace,
};

use super::types::OutboxError;

/// One entity to enqueue.
///
/// ```ignore
/// let msg = Record::to("orders", 3)
///     .payload(bytes, "application/json")
///     .trace("order-4711")
///     .build()?;
/// outbox.enqueue(&txn, msg).await?;
/// ```
#[derive(Debug)]
pub struct Record<'a> {
    queue: &'a str,
    item: RecordItem<'a>,
    trace: Option<&'a str>,
}

impl<'a> Record<'a> {
    /// Name the queue and partition this entity goes to.
    #[must_use]
    pub const fn to(queue: &'a str, partition: u32) -> RecordTarget<'a> {
        RecordTarget { queue, partition }
    }

    /// The queue this entity goes to.
    #[must_use]
    pub const fn queue(&self) -> &'a str {
        self.queue
    }

    /// Payload bytes this entity contributes to its queue's backlog.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.item.payload.len()
    }

    pub(crate) fn into_parts(self) -> (&'a str, RecordItem<'a>, Option<&'a str>) {
        (self.queue, self.item, self.trace)
    }
}

/// A queue and partition awaiting a payload.
#[derive(Debug)]
pub struct RecordTarget<'a> {
    queue: &'a str,
    partition: u32,
}

impl<'a> RecordTarget<'a> {
    /// Supply the payload and its type.
    #[must_use]
    pub fn payload(self, payload: Vec<u8>, payload_type: &'a str) -> RecordBuilder<'a> {
        RecordBuilder {
            queue: self.queue,
            item: RecordItem {
                partition: self.partition,
                payload,
                payload_type,
            },
            trace: None,
        }
    }
}

/// A complete entity, awaiting optional parts and validation.
#[derive(Debug)]
pub struct RecordBuilder<'a> {
    queue: &'a str,
    item: RecordItem<'a>,
    trace: Option<&'a str>,
}

impl<'a> RecordBuilder<'a> {
    /// Make this entity traceable under a caller-supplied trace.
    ///
    /// An entity with no trace is not traceable and records nothing.
    #[must_use]
    pub const fn trace(mut self, trace: &'a str) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Validate and finish.
    ///
    /// # Errors
    ///
    /// Returns an error if the queue name, payload type, payload size or trace
    /// breaks its rule. No database statement has run at this point.
    pub fn build(self) -> Result<Record<'a>, OutboxError> {
        validate_queue_name(self.queue)?;
        validate_payload_type(self.item.payload_type)?;
        validate_payload(&self.item.payload)?;
        if let Some(trace) = self.trace {
            validate_trace(trace)?;
        }

        Ok(Record {
            queue: self.queue,
            item: self.item,
            trace: self.trace,
        })
    }
}

/// A batch of entities that share a queue, a default payload type and a trace.
///
/// The trace belongs to the batch rather than to an entity: a batch completes
/// once every entity in it has reached a terminal state, and that is the one
/// thing a caller is told.
///
/// ```ignore
/// let batch = Records::to("orders")
///     .payload_type("application/json")
///     .trace("import-2026-09-08")
///     .push(0, first)
///     .push_with_type(1, second, "application/vnd.legacy+json")
///     .build()?;
/// outbox.enqueue_batch(&txn, batch).await?;
/// ```
#[derive(Debug)]
pub struct Records<'a> {
    queue: &'a str,
    items: Vec<RecordItem<'a>>,
    trace: Option<&'a str>,
}

impl<'a> Records<'a> {
    /// Name the queue this batch goes to.
    #[must_use]
    pub const fn to(queue: &'a str) -> RecordsTarget<'a> {
        RecordsTarget { queue }
    }

    /// The queue this batch goes to.
    #[must_use]
    pub const fn queue(&self) -> &'a str {
        self.queue
    }

    pub(crate) fn into_parts(self) -> (&'a str, Vec<RecordItem<'a>>, Option<&'a str>) {
        (self.queue, self.items, self.trace)
    }

    /// Number of entities in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the batch has no entities.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Total payload bytes across the batch.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.items.iter().map(|i| i.payload.len()).sum()
    }
}

/// A queue awaiting the batch's default payload type.
#[derive(Debug)]
pub struct RecordsTarget<'a> {
    queue: &'a str,
}

impl<'a> RecordsTarget<'a> {
    /// Supply the payload type every entity in this batch uses unless it
    /// overrides it.
    #[must_use]
    pub const fn payload_type(self, payload_type: &'a str) -> RecordsBuilder<'a> {
        RecordsBuilder {
            queue: self.queue,
            payload_type,
            items: Vec::new(),
            trace: None,
        }
    }
}

/// A batch under construction.
#[derive(Debug)]
pub struct RecordsBuilder<'a> {
    queue: &'a str,
    payload_type: &'a str,
    items: Vec<RecordItem<'a>>,
    trace: Option<&'a str>,
}

impl<'a> RecordsBuilder<'a> {
    /// Make this batch traceable under a caller-supplied trace.
    ///
    /// A batch with no trace is not traceable and records nothing.
    #[must_use]
    pub const fn trace(mut self, trace: &'a str) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Add an entity using the batch's default payload type.
    #[must_use]
    pub fn push(mut self, partition: u32, payload: Vec<u8>) -> Self {
        let payload_type = self.payload_type;
        self.items.push(RecordItem {
            partition,
            payload,
            payload_type,
        });
        self
    }

    /// Add an entity with a payload type of its own.
    #[must_use]
    pub fn push_with_type(
        mut self,
        partition: u32,
        payload: Vec<u8>,
        payload_type: &'a str,
    ) -> Self {
        self.items.push(RecordItem {
            partition,
            payload,
            payload_type,
        });
        self
    }

    /// Validate and finish.
    ///
    /// Every entity is validated before any is accepted, matching batch
    /// enqueue being all-or-nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the queue name, the trace, or any entity's payload
    /// type or payload size breaks its rule. No database statement has run at
    /// this point.
    pub fn build(self) -> Result<Records<'a>, OutboxError> {
        validate_queue_name(self.queue)?;
        if let Some(trace) = self.trace {
            validate_trace(trace)?;
            // A trace is a request to be told when the batch finishes, and a
            // batch with no entities never will: nothing acks it, so nothing
            // counts it down, so nothing stamps its completion and whoever
            // subscribed waits for ever. An untraced empty batch stays legal -
            // it asks for nothing and gets nothing.
            if self.items.is_empty() {
                return Err(OutboxError::EmptyTracedBatch);
            }
        }
        for item in &self.items {
            validate_payload_type(item.payload_type)?;
            validate_payload(&item.payload)?;
        }

        Ok(Records {
            queue: self.queue,
            items: self.items,
            trace: self.trace,
        })
    }
}

/// One entity's contribution to a submission, as the write path consumes it.
#[derive(Debug)]
pub struct RecordItem<'a> {
    pub partition: u32,
    pub payload: Vec<u8>,
    pub payload_type: &'a str,
}
