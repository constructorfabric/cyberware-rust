//! Output ports the domain depends on.

pub mod coordination;
pub mod metrics;

pub use coordination::{
    CoordinatorBinding, LeaderWork, LeaderWorkFuture, SingletonCoordinator, SingletonScope,
};
pub use metrics::{DenialReason, NoopMetrics, QeMetrics};
