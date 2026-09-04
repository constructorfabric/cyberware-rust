//! Domain layer of the quota-enforcement gear.

pub mod admission;
pub mod bootstrap;
pub mod error;
pub mod pep;
pub mod plugins;
pub mod ports;
pub mod readiness;
pub mod service;

pub use admission::{Admission, AdmissionTarget, Admitted};
pub use bootstrap::{Bootstrap, Bound};
pub use error::{Dependency, DomainError, PluginKind, ResourceKind};
pub use plugins::PluginBinding;
pub use ports::{CoordinatorBinding, LeaderWork, SingletonCoordinator, SingletonScope};
pub use readiness::{Readiness, ReadinessState};
pub use service::Service;
