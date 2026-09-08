//! Domain layer of the quota-enforcement gear.

pub mod admission;
pub mod attribution;
pub mod bootstrap;
pub mod catalog;
pub mod error;
pub mod pep;
pub mod plugins;
pub mod ports;
pub mod readiness;
pub mod service;
pub mod tokens;

pub use admission::{Admission, AdmissionTarget, Admitted};
pub use attribution::{AdmittedEvaluation, Attribution, MappedAttribution, PolicyInput};
pub use bootstrap::{Bootstrap, Bound, CatalogBinding};
pub use catalog::{CatalogBuilder, CatalogConfig, ProjectionContractCatalog};
pub use error::{Dependency, DomainError, PluginKind, ResourceKind};
pub use plugins::PluginBinding;
pub use ports::{CoordinatorBinding, LeaderWork, SingletonCoordinator, SingletonScope};
pub use readiness::{Readiness, ReadinessState};
pub use service::Service;
