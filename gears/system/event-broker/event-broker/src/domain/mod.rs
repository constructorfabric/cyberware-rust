//! Domain layer: business logic, service traits, and repository contracts.
//! No infra dependencies (`DESIGN.md` §1.3 Architecture Layers).

pub mod authz;
pub mod backend;
pub mod cluster;
pub mod consumer_group_coordinator;
pub mod delivery;
pub mod error;
pub mod event_type;
pub mod idempotency;
pub mod ingest;
pub mod model;
pub mod notify;
pub mod outbox;
pub mod projection;
pub mod repo;
pub mod resolution;
pub mod specification;
pub mod streaming;

#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod outbox_tests;

pub use cluster::EventBrokerCluster;
pub use consumer_group_coordinator::ConsumerGroupCoordinator;
pub use delivery::DeliveryService;
pub use error::DomainError;
pub use ingest::IngestService;
pub use specification::SpecificationManager;
