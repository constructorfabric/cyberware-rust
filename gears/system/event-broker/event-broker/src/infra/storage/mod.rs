//! `Storage` facade (`ConsumerGroupRepo`/`CursorRepo`/`SubscriptionRepo`/
//! `ActiveStreamMarker`/`DeliveryNotifier`).
//!
//! The event log itself is not here. It belongs to whichever
//! `EventBrokerBackend` a topic's settings name - the `SQLite` one lives in its
//! own crate - and backend RESOLUTION lives in `domain::backend`
//! (`BackendResolver`/`SingleBackendResolver`).

pub mod entity;
pub mod error;
pub mod migrations;
pub mod storage;

pub use storage::Storage;
