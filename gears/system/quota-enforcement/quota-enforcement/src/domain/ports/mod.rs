//! Output ports the domain depends on.

pub mod contracts;
pub mod coordination;
pub mod metrics;
pub mod pdp;

pub use contracts::{ContractRegistry, DiscoveredType, RegisteredType};
pub use coordination::{
    CoordinatorBinding, LeaderWork, LeaderWorkFuture, SingletonCoordinator, SingletonScope,
};
pub use metrics::{DenialReason, NoopMetrics, QeMetrics, ValidationReason, ValidationSurface};
pub use pdp::PdpProbe;
