//! Infrastructure adapters: the cluster coordination adapter, the
//! canonical-error lift, and the metrics meter.

pub mod canonical_mapping;
pub mod cluster_coordination;
pub mod metrics;

pub use cluster_coordination::{
    ClusterCoordination, ClusterCoordinationBinding, ElectionTiming, QuotaEnforcementProfile,
    SCOPE_PREFIX,
};
pub use metrics::{QeMetricsMeter, build_default_adapter};
