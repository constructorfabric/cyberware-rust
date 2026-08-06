//! Delivery wake-up notifications (design.md D6): a `ClusterCacheV1`-backed,
//! payload-free "something changed, go re-check" signal, replacing
//! `domain/delivery.rs`'s old temporary polling loop. The consumer wakes on
//! the bare notification and re-queries the backend directly - the
//! notification itself carries no payload and is never the source of truth.
//!
//! **Correction against design.md D6's literal wording**: D6 (and task 7.2)
//! describe watching one exact key per `(topic, partition)` a subscription
//! is assigned to. A subscription can be assigned many partitions across
//! several topics, which would mean racing an unbounded number of
//! concurrent `ClusterCacheV1::watch` calls per open stream just to wake on
//! whichever fires first. Since the wake is payload-free and re-query is
//! authoritative regardless of which key fired, this instead watches the
//! single shared [`NOTIFICATION_PREFIX`] namespace via `watch_prefix` -
//! exactly one watch per open stream, at the cost of an occasional harmless
//! extra re-query pass when an unrelated topic's notification fires. The
//! per-`(topic, partition)` key granularity is kept on the *write* side
//! ([`notification_key`]) so a future move to per-topic or per-partition
//! watching (e.g. once a subscription's own topic set is small and stable
//! enough to make N-way racing worthwhile) doesn't need an ingest-side
//! change.

use std::time::Duration;

use async_trait::async_trait;

/// The shared `ClusterCacheV1` namespace (already `evbk`-scoped by
/// `EventBrokerCluster::resolve`) every ingest notification is written
/// under, and delivery watches as a whole via `watch_prefix`.
pub const NOTIFICATION_PREFIX: &str = "notif";

/// Builds one `(topic, partition)`'s notification key from the topic's
/// deterministic GTS UUID.
///
/// `ClusterCacheV1` key names must match `[a-zA-Z0-9_-]+(/[a-zA-Z0-9_-]+)*`,
/// and a GTS identifier carries `.` and `~`, so the identifier cannot be the
/// key. What replaces it has to be an identity every instance derives the
/// same way without coordinating: `GtsId::to_uuid` is a UUID v5 under a fixed
/// GTS namespace, so the same identifier always yields the same value, in any
/// process, with no registry call and no database read. It is the identity
/// `types-registry` itself stores (`GtsInstance::uuid`).
///
/// Deliberately **not** the integer surrogate id from the specification cache,
/// which this used to be. That column is an auto-increment: stable inside one
/// database, and not something a federated or sharded ingest and delivery pair
/// can be trusted to agree on. Two instances assigning one topic different
/// integers would publish to a key nobody watches, and the failure is silent -
/// a stream that simply never wakes.
///
/// `None` only where `topic` does not parse as a GTS identifier, which the type
/// itself is supposed to make impossible; the caller logs and skips rather than
/// bringing the process down over it.
#[must_use]
pub fn notification_key(topic: &toolkit_gts::GtsInstanceId, partition: i32) -> Option<String> {
    let uuid = gts::GtsId::try_new(topic.as_ref()).ok()?.to_uuid();
    Some(format!(
        "{NOTIFICATION_PREFIX}/{}/{partition}",
        uuid.simple()
    ))
}

/// Implemented by `Storage` (the same object already holding the
/// `ClusterCacheV1` handle for the `subscription` namespace) so
/// `domain/delivery.rs`'s stream loop can wake on ingest activity without
/// depending on `cluster_sdk` directly (`domain/` has no infra
/// dependencies).
#[async_trait]
pub trait DeliveryNotifier: Send + Sync {
    /// Waits for any ingest notification, or `timeout`, whichever comes
    /// first. Never errors: a notification-backend failure degrades to
    /// "timed out" rather than propagating, since the delivery loop's own
    /// re-query decides what's actually new either way - a missed or failed
    /// wake costs one extra iteration, not a correctness bug.
    async fn wait_for_notification(&self, timeout: Duration);
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod notify_tests;
