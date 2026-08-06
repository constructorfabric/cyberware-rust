use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::api::Position;
use crate::api::{EventBrokerBackend, RetentionReport, RetentionRequest};
use crate::error::StorageBackendError;
use crate::models::Event;
use crate::models::{PartitionLeader, PartitionRange, TopicSegment};
use crate::sequence::Sequence;

use super::core::MockBroker;
use super::ingest::ingest_one;

/// The mock implements `EventBrokerBackend` over the same `Core.log` as
/// the transport seam (O2 - flattening playground). `persist` goes through the
/// same `ingest_one` pipeline so storage and transport stay in sync.
///
/// `truncate` and `segments` are stubs marked with `TODO(M5b-flattening)` -
/// they will become the primary surface when the backend-contract is reshaped to
/// the PRD's `persist/query/truncate/segments` form.
///
/// TODO(M5b-flattening): the storage-backend contract is intentionally NOT yet
/// reconciled with the design docs. The docs (DESIGN §3.2 / PRD) describe the
/// intended shape - trait `StorageBackend`, methods `persist/query/truncate/segments`,
/// RFC 9457 `StorageError` - while this SDK currently ships `EventBrokerBackend` with
/// `persist/read/query/list_partition_leaders` and a plain `StorageBackendError`
/// (note `query`/`read` are swapped vs the docs' names). Reconcile the name, method
/// set, and error model here during the M5b backend reshape - it's a real API + wire
/// change that belongs in its own change, not a docs/review-fixes pass.
#[async_trait]
impl EventBrokerBackend for MockBroker {
    async fn persist(
        &self,
        _ctx: &SecurityContext,
        // Ingest resolves the destination stream from each event's type, so the
        // caller's routing hint is not consulted.
        _topic: &str,
        _partition: u32,
        events: &[Event],
    ) -> Result<(), StorageBackendError> {
        {
            let faults = self.faults.lock().await;
            if let Some(reason) = &faults.reject_persist {
                return Err(StorageBackendError::PersistFailed {
                    reason: reason.clone(),
                    detail: "fault injected via MockBrokerHandle::reject_persist".to_owned(),
                    instance: String::new(),
                });
            }
        }
        let mut core = self.core.lock().await;
        for event in events {
            ingest_one(&mut core, event).map_err(|err| StorageBackendError::PersistFailed {
                reason: err.to_string(),
                detail: String::new(),
                instance: String::new(),
            })?;
        }
        self.notify.notify_waiters();
        Ok(())
    }

    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        position: Position,
    ) -> Result<Sequence, StorageBackendError> {
        let core = self.core.lock().await;
        super::transport::resolve_position(&core, topic, partition, position)
    }

    async fn read(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        after: Sequence,
        max_count: usize,
    ) -> Result<Vec<Event>, StorageBackendError> {
        let core = self.core.lock().await;
        Ok(core
            .topics
            .get(topic)
            .map(|t| {
                t.read(partition, after, max_count)
                    .into_iter()
                    .map(|se| se.event.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn query(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
        partition: u32,
        _range: PartitionRange,
    ) -> Result<Vec<TopicSegment>, StorageBackendError> {
        let core = self.core.lock().await;
        let t = match core.topics.get(topic) {
            Some(t) => t,
            None => return Ok(vec![]),
        };
        let log = t.log.get(&partition);
        Ok(if let Some(events) = log {
            if events.is_empty() {
                vec![]
            } else {
                let start = events
                    .first()
                    .and_then(|e| e.event.sequence)
                    .unwrap_or(Sequence::NONE);
                let end = events
                    .last()
                    .and_then(|e| e.event.sequence)
                    .unwrap_or(Sequence::NONE);
                let ts = events.first().and_then(|e| e.event.sequence_time);
                let te = events.last().and_then(|e| e.event.sequence_time);
                vec![TopicSegment {
                    topic: topic.to_owned(),
                    partition,
                    start_sequence: start,
                    end_sequence: end,
                    start_time: ts,
                    end_time: te,
                    segments: vec![],
                }]
            }
        } else {
            vec![]
        })
    }

    async fn list_partition_leaders(
        &self,
        _ctx: &SecurityContext,
        topic: &str,
    ) -> Result<Vec<PartitionLeader>, StorageBackendError> {
        let core = self.core.lock().await;
        let partitions = core
            .topics
            .get(topic)
            .map(|state| state.partitions)
            .unwrap_or(0);
        Ok((0..partitions)
            .map(|p| PartitionLeader {
                partition: p,
                endpoint: "mock://in-process".to_owned(),
            })
            .collect())
    }

    /// Reports the partition as it stands and removes nothing.
    ///
    /// The mock's log is a `Vec` addressed by index, with offset equal to
    /// index, so removing a prefix would silently renumber every event still
    /// in it and every committed cursor pointing at one. There is also nothing
    /// for retention to protect here: the log lives in the test process's
    /// memory and goes away with it.
    ///
    /// The counts are still real, so a caller that asserts on the report
    /// against this double is asserting on something true.
    async fn maintain(
        &self,
        _ctx: &SecurityContext,
        request: &RetentionRequest,
    ) -> Result<RetentionReport, StorageBackendError> {
        let core = self.core.lock().await;
        let stored = core
            .topics
            .get(request.topic())
            .and_then(|t| t.log.get(&request.partition()));
        let Some(stored) = stored else {
            return Ok(RetentionReport::default());
        };
        Ok(RetentionReport {
            removed_events: 0,
            removed_bytes: 0,
            remaining_events: stored.len() as u64,
            // The payloads, which is all a figure can honestly mean here: the
            // mock stores nothing, so there is no stored row whose size this
            // could report.
            remaining_bytes: stored
                .iter()
                .filter_map(|e| e.event.data.as_ref())
                .filter_map(|data| serde_json::to_vec(data).ok())
                .map(|bytes| bytes.len() as u64)
                .sum(),
            oldest_surviving_sequence: stored.first().and_then(|e| e.event.sequence),
        })
    }
}
