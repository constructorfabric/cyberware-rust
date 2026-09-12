//! Output ports the domain depends on.

pub mod contracts;
pub mod coordination;
pub mod lifecycle_gauges;
pub mod metric_registry;
pub mod metrics;
pub mod pdp;

pub use contracts::{ContractRegistry, DiscoveredType, RegisteredType};
pub use coordination::{
    CoordinatorBinding, LeaderWork, LeaderWorkFuture, SingletonCoordinator, SingletonScope,
};
pub use lifecycle_gauges::{LifecycleCounts, LifecycleGaugeSink, NoopGaugeSink};
pub use metric_registry::{Classified, Freshness, MetricDescriptor, MetricMode, MetricRegistry};
pub use metrics::{DenialReason, NoopMetrics, QeMetrics, ValidationReason, ValidationSurface};
pub use pdp::PdpProbe;
