//! The Quota lifecycle (`features/quota-lifecycle.md`): create, update,
//! deactivate, and read, with the validation chain that runs before storage.
//!
//! - [`request`]: the transport-agnostic, presence-aware request models.
//! - [`validation`]: the draft validation algorithm and the update and list
//!   gates.
//! - [`metadata`]: metadata validation against the owner's constraint
//!   contract.
//! - [`window`]: the validity-window view computed at read time.
//! - [`events`]: the `quota-changed` event every mutation enqueues.
//! - [`service`]: the component that orders the steps.
//! - [`gauges`]: the leader-only refresh of the lifecycle gauges.

pub mod events;
pub mod gauges;
pub mod metadata;
pub mod request;
pub mod service;
pub mod validation;
pub mod window;

pub use events::ChangeKind;
pub use gauges::{GaugeTiming, LifecycleGaugeRefresher, RefreshError};
pub use request::{CreateQuotaRequest, ListQuotasRequest, Presence, UpdateQuotaRequest};
pub use service::QuotaManagement;
pub use validation::QuotaLimits;
