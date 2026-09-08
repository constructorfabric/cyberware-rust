//! # Kubernetes cluster plugin
//!
//! `k8s_cluster_plugin` is the Kubernetes backend plugin for the cluster gear
//! (DESIGN.md §1). It provides **three native primitives** — a
//! `LeaderElectionBackend` and a `DistributedLockBackend` over
//! `coordination.k8s.io/v1.Lease`, and a `ClusterCacheBackend` over a
//! purpose-built `ClusterCacheEntry` custom resource — with no SDK default in
//! the picture. It is the first plugin to register a native
//! `with_leader_election_provider` in the cluster gear.
//!
//! ## Lifecycle (outbox-style builder/handle, ADR-006)
//!
//! Like the postgres and standalone plugins, this plugin is **not** a
//! `RunnableCapability`. It exposes a builder/handle pair owned by the cluster
//! wiring crate (`cf-gears-cluster`), plus three standalone per-primitive shapes
//! (DESIGN.md §3.2, §3.5).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

mod cache;
mod client;
mod config;
mod crd;
mod guarded;
mod k8s_error;
mod leader;
mod lease;
mod lock;
mod naming;
mod observed;
mod plugin;
mod preflight;
mod provider;
mod shutdown;

// Public API re-exports (Phase 4 complete), mirroring the postgres plugin's
// `lib.rs`.
pub use config::{
    K8sCacheConfig, K8sClusterConfig, K8sLeaderElectionConfig, K8sLockConfig, ReadMode,
};
pub use crd::{ClusterCacheEntry, ClusterCacheEntrySpec};
pub use plugin::{
    K8sCacheBuilder, K8sCacheHandle, K8sCachePlugin, K8sClusterBuilder, K8sClusterHandle,
    K8sClusterPlugin, K8sLeaderElectionBuilder, K8sLeaderElectionHandle, K8sLeaderElectionPlugin,
    K8sLockBuilder, K8sLockHandle, K8sLockPlugin,
};
pub use provider::{K8sCacheProvider, K8sLeaderElectionProvider, K8sLockProvider, PROVIDER_NAME};
