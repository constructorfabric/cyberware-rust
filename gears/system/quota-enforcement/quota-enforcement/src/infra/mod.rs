//! Infrastructure adapters: the cluster coordination adapter, the PDP
//! reachability probe, the canonical-error lift, and the metrics meter.

pub mod canonical_mapping;
pub mod cluster_coordination;
pub mod metrics;
pub mod pdp_probe;

pub use cluster_coordination::{
    ClusterCoordination, ClusterCoordinationBinding, ElectionTiming, QuotaEnforcementProfile,
    SCOPE_PREFIX,
};
pub use metrics::{QeMetricsMeter, build_default_adapter};
pub use pdp_probe::{PROBE_SUBJECT, PdpReachability};
