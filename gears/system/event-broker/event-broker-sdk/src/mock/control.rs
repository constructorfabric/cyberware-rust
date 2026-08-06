use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use crate::ids::{ConsumerGroupId, ProducerId, SubscriptionId};
use crate::sequence::Sequence;

/// A `(topic, partition)` pair from a subscription assignment.
/// Returned by [`MockBrokerHandle::assignment`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartitionSlot {
    pub topic: String,
    pub partition: u32,
}

use super::core::{Core, EventTypeReg, FaultConfig, MockBroker, StoredEvent, TopicState};

/// Test-facing control API over `MockBroker`.
///
/// Obtained via `MockBrokerHandle::new(mock)` or `MockBroker::handle()`.
/// Provides setup, fault injection, and assertion helpers without going through
/// the transport trait.
#[derive(Clone, Debug)]
pub struct MockBrokerHandle {
    core: Arc<Mutex<Core>>,
    faults: Arc<Mutex<FaultConfig>>,
}

/// Which event type to repoint, and where. A struct rather than two `&str`
/// arguments, so neither can be passed in the other's place.
pub struct PartitionKeyFixture<'a> {
    pub event_type: &'a str,
    pub pointer: &'a str,
}

impl MockBrokerHandle {
    pub fn from_broker(broker: &MockBroker) -> Self {
        Self {
            core: broker.core.clone(),
            faults: broker.faults.clone(),
        }
    }

    // -- Setup -----------------------------------------------------------------

    /// Register a topic. `id` must be a GTS topic **instance** identifier:
    /// `gts.cf.core.events.topic.v1~<vendor>.<...>.v1`. What is stored is the
    /// instance document `types-registry` would provision. `partitions` is the
    /// mock's own fixture knob standing in for the broker's configuration, since
    /// a topic carries no partition count.
    ///
    /// # Panics
    /// Panics if `id` is not a valid GTS instance identifier (must start with
    /// `gts.` and not end with `~`) or if `partitions` is zero.
    pub async fn register_topic(&self, id: &str, partitions: u32) {
        assert_gts_topic(id);
        assert!(partitions > 0, "topic partitions must be greater than zero");

        let mut core = self.core.lock().await;
        core.topics
            .entry(id.to_owned())
            .or_insert_with(|| TopicState::new(id, partitions));
    }

    /// Repoints a registered event type's partition key, for fixtures that need
    /// events spread across partitions. Under the default pointer every event of
    /// one tenant lands on one partition, which is the contract - so a test
    /// exercising several partitions declares a pointer at a member it varies.
    ///
    /// # Panics
    /// Panics if no registered event type carries `event_type`.
    pub async fn set_partition_key(&self, fixture: PartitionKeyFixture<'_>) {
        let PartitionKeyFixture {
            event_type,
            pointer,
        } = fixture;
        let mut core = self.core.lock().await;
        let reg = core
            .topics
            .values_mut()
            .find_map(|state| state.event_types.get_mut(event_type))
            .expect("mock: set_partition_key needs a registered event type");
        reg.schema["x-gts-traits"]["partition_key"] = Value::String(pointer.to_owned());
    }

    /// Provision a NAMED consumer group - the `types_registry` startup-upsert
    /// analog. Named groups are not minted via `POST /v1/consumer-groups`; module
    /// code registers them at startup. A subsequent JOIN to the named identifier
    /// is then permitted (the `:consume` grant is an HTTP-layer authz concern,
    /// out of the mock's scope).
    pub async fn register_named_group(&self, gts_id: &str) {
        let mut core = self.core.lock().await;
        core.groups_registry.insert(
            ConsumerGroupId::from_gts(gts_id),
            super::core::GroupReg {
                kind: crate::models::ConsumerGroupKind::Named,
                owner_tenant: uuid::Uuid::nil(),
                // A types-registry-provisioned group has no real principal uuid
                // in the mock; the nil uuid stands in for it.
                owner_principal: uuid::Uuid::nil(),
            },
        );
    }

    /// Register an event type on an already-registered topic. `data_schema` is
    /// the payload contract; it is stored as the `data` narrowing of a derived
    /// event-type schema document, together with `topic` and `allowed_subjects`
    /// as that document's `x-gts-traits`.
    ///
    /// Both `topic` and `type_id` must be GTS identifiers.
    ///
    /// # Panics
    /// Panics if either identifier is not a valid GTS identifier.
    pub async fn register_event_type(
        &self,
        topic: &str,
        type_id: &str,
        data_schema: Value,
        allowed_subjects: &[&str],
    ) {
        assert_gts_topic(topic);
        assert_gts_event_type(type_id);
        let mut core = self.core.lock().await;
        if let Some(t) = core.topics.get_mut(topic) {
            t.event_types.insert(
                type_id.to_owned(),
                EventTypeReg {
                    schema: crate::gts::derived_event_type_schema(
                        type_id,
                        topic,
                        data_schema,
                        allowed_subjects,
                    ),
                },
            );
        }
    }

    /// Remove a producer registration and its cursor state.
    pub async fn forget_producer(&self, producer_id: ProducerId) {
        let mut core = self.core.lock().await;
        core.producers.remove(&producer_id);
        core.producer_state
            .retain(|(pid, _, _), _| *pid != producer_id);
    }

    // -- Fault injection -------------------------------------------------------

    /// Cause the next `stream()` poll for this subscription to return a 410-equivalent error.
    pub async fn inject_gone(&self, sub: SubscriptionId) {
        self.faults.lock().await.force_gone.insert(sub);
    }

    /// Cause the next `stream()` poll for this subscription to return a 404-equivalent error.
    pub async fn inject_not_found(&self, sub: SubscriptionId) {
        self.faults.lock().await.force_not_found.insert(sub);
    }

    /// Immediately fire session_timeout for this subscription, triggering a rebalance.
    /// Simulates a crash (C6) or standby takeover (C9) without waiting for real wall-clock expiry.
    pub async fn expire_subscription(&self, sub_id: SubscriptionId) {
        let mut core = self.core.lock().await;
        let group_id = match core.subscriptions.get(&sub_id).map(|s| s.group) {
            Some(g) => g,
            None => return,
        };
        core.subscriptions.remove(&sub_id);
        if let Some(group) = core.groups.get_mut(&group_id) {
            group.members.retain(|m| *m != sub_id);
        }
        super::rebalance::run_rebalance(&group_id, &mut core);
    }

    /// Force a rebalance on a group (direct trigger, no membership change).
    pub async fn force_rebalance(&self, group: &ConsumerGroupId) {
        let mut core = self.core.lock().await;
        super::rebalance::run_rebalance(group, &mut core);
    }

    /// Reject the next `persist` / `publish` call with an error (M3 chain-gap surface).
    /// Pass `None` to clear the rule.
    pub async fn reject_persist(&self, reason: Option<&str>) {
        self.faults.lock().await.reject_persist = reason.map(str::to_owned);
    }

    /// Set the producer publish rate-limit allowance. `Some(n)` lets the next
    /// `n` publishes through (single publish, or per-event within a batch), then
    /// further publishes return `EventBrokerError::RateLimited` (429-equivalent).
    /// `Some(0)` refuses the very next publish; `None` clears the limit.
    pub async fn set_publish_rate_limit(&self, limit: Option<u32>) {
        self.faults.lock().await.publish_rate_limit = limit;
    }

    /// Set the heartbeat interval for stream tests. Default is 5s; set to a tiny value
    /// for tests that need to observe a heartbeat quickly.
    pub async fn set_heartbeat_interval(&self, d: std::time::Duration) {
        self.faults.lock().await.heartbeat_interval = d;
    }

    // -- Assertions ------------------------------------------------------------

    /// Current `cursor.offset` for `(group, topic, partition)`, or `None` if not set.
    pub async fn cursor(
        &self,
        group: &ConsumerGroupId,
        topic: &str,
        partition: u32,
    ) -> Option<Sequence> {
        self.core
            .lock()
            .await
            .groups
            .get(group)
            .and_then(|g| g.cursor.get(&(topic.to_owned(), partition)))
            .map(|c| c.offset)
    }

    /// Current `cursor.last_examined` for `(group, topic, partition)`, or `None` if not set.
    pub async fn last_examined(
        &self,
        group: &ConsumerGroupId,
        topic: &str,
        partition: u32,
    ) -> Option<Sequence> {
        self.core
            .lock()
            .await
            .groups
            .get(group)
            .and_then(|g| g.cursor.get(&(topic.to_owned(), partition)))
            .map(|c| c.last_examined)
    }

    /// Partitions currently assigned to a subscription.
    pub async fn assignment(&self, sub: SubscriptionId) -> Vec<PartitionSlot> {
        self.core
            .lock()
            .await
            .subscriptions
            .get(&sub)
            .map(|s| {
                s.assigned
                    .iter()
                    .map(|(topic, partition)| PartitionSlot {
                        topic: topic.clone(),
                        partition: *partition,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Active member subscription ids in a group.
    pub async fn members(&self, group: &ConsumerGroupId) -> Vec<SubscriptionId> {
        self.core
            .lock()
            .await
            .groups
            .get(group)
            .map(|g| g.members.clone())
            .unwrap_or_default()
    }

    /// All stored events on a `(topic, partition)`.
    pub async fn stored(&self, topic: &str, partition: u32) -> Vec<StoredEvent> {
        self.core
            .lock()
            .await
            .topics
            .get(topic)
            .and_then(|t| t.log.get(&partition))
            .cloned()
            .unwrap_or_default()
    }

    /// Current `topology_version` for a group.
    pub async fn topology_version(&self, group: &ConsumerGroupId) -> i64 {
        self.core
            .lock()
            .await
            .groups
            .get(group)
            .map(|g| g.topology_version)
            .unwrap_or(0)
    }
}

// -- GTS format validation -----------------------------------------------------

/// Assert that a string is a valid GTS identifier of the expected kind, using the
/// `gts-id` library.
///
/// A wrong-kind identifier is a caller mistake worth failing on here rather than
/// several layers down where the document is assembled.
///
/// # Panics
/// Panics with the parse error if `id` is not a valid GTS identifier, or if its
/// kind is not the expected one.
fn assert_gts_kind(id: &str, context: &str, expect_type: bool) {
    match gts_id::GtsId::try_new(id) {
        Err(e) => panic!("mock: {context} must be a GTS identifier, got {id:?}: {e}"),
        Ok(parsed) => {
            let (kind, shape) = if expect_type {
                ("type", "ending in `~`")
            } else {
                ("instance", "not ending in `~`")
            };
            assert!(
                parsed.is_type() == expect_type,
                "mock: {context} must be a GTS {kind} identifier {shape}, got {id:?}"
            );
        }
    }
}

pub(super) fn assert_gts_topic(id: &str) {
    assert_gts_kind(id, "topic id", false);
}

pub(super) fn assert_gts_event_type(id: &str) {
    assert_gts_kind(id, "event type id", true);
}

impl MockBroker {
    /// Get a test-facing handle for setup, fault injection, and assertions.
    pub fn handle(&self) -> MockBrokerHandle {
        MockBrokerHandle::from_broker(self)
    }
}
