//! Infrastructure adapters: the cluster coordination adapter, the PDP
//! reachability probe, the projection contract registry, the canonical-error
//! lift, and the metrics meter.

pub mod canonical_mapping;
pub mod cluster_coordination;
pub mod metrics;
pub mod pdp_probe;
pub mod types_registry;

pub use cluster_coordination::{
    ClusterCoordination, ClusterCoordinationBinding, ElectionTiming, QuotaEnforcementProfile,
    SCOPE_PREFIX,
};
pub use metrics::{
    ADMITTED_METRIC_VIOLATIONS_TOTAL, CONTRACT_VALIDATION_FAILURES_TOTAL, DENIAL_TOTAL,
    QeMetricsMeter, build_default_adapter,
};
pub use pdp_probe::{PROBE_SUBJECT, PdpReachability};
pub use types_registry::{DEFAULT_REGISTRY_DEADLINE, TypesRegistryContracts};
