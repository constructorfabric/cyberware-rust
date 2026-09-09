//! # Cluster gear
//!
//! `cluster` (`cf-gears-cluster`) is the cluster gear (DESIGN §3.4 / §3.7,
//! component `cpt-cf-clst-component-wiring`). It registers the per-profile,
//! per-primitive coordination backends produced by cluster plugins into the
//! `ClientHub` — under the stable `cluster:{profile}` scope the SDK resolvers look
//! them up in — and owns the cluster lifecycle.
//!
//! The crate plays two roles, in line with the platform's one-gear-per-domain
//! layout (`<gear>-sdk` + `<gear>` + plugins):
//!
//! 1. **The gear** — a `RunnableCapability` (`name = "cluster"`) whose `start`
//!    builds the wiring from operator config and whose `stop` tears it down. See
//!    the private `gear` module.
//! 2. **An embeddable library** — [`ClusterWiring::builder`]`(hub).…build_and_start()
//!    ->` [`ClusterHandle`] (and [`ClusterWiring::from_config`]) are `pub`, so a
//!    consumer gear may own the wiring directly instead of depending on the
//!    `cluster` gear. [`ClusterHandle::stop`] is the single shutdown entry point.
//!
//! DESIGN §3.7 originally specified the wiring as a non-gear library owned by a
//! separate host gear (the outbox analogy). That was collapsed into this single
//! gear crate — the builder/handle library still exists and is embeddable, but the
//! reusable surface is `cluster-sdk`, so a dedicated wiring crate added a third
//! core crate no other gear has. See DESIGN §3.7 (amended).
//!
//! # Crate layout, and where it departs from the platform template
//!
//! The canonical layout is `docs/toolkit_unified_system/02_gear_layout_and_sdk_pattern.md`;
//! the out-of-process variant is `09_oop_grpc_sdk_pattern.md`. **The two disagree**, and where they
//! do, this crate follows what shipped gears actually do. Each departure below is
//! deliberate, and carries the citation it was decided against.
//!
//! - **`api/grpc/` is a directory**, where `09:132` prescribes a single
//!   `src/grpc_server.rs`. No file by that name exists anywhere in the repo; the only
//!   other out-of-process gear (`examples/oop-gears/calculator/`) also uses an `api/grpc/`
//!   directory. `S4` adds `api/rest/` beside it, which is `02:39`'s shape.
//! - **No `infra/`.** This gear owns no store — a backend's persistence belongs to the
//!   plugin implementing it. `calculator` and `nodes-registry` ship without one too.
//! - **[`defaults`] is an unprecedented module name** (no other gear has one). Kept
//!   because it is DESIGN §3.11's own term for what it holds — the "implement cache
//!   only, get all three primitives" backends — and because it is a public path that
//!   plugins' conformance tests import.
//! - **The binary lives in this crate** (`main.rs` + `[[bin]]`), per `09:133` and
//!   matching `calculator`. `users-info`'s separate `-server` crate is the in-process
//!   pattern. Unlike `calculator`, the `[[bin]]` carries no `required-features` — see
//!   the note on it in `Cargo.toml`.
//! - **A third crate, `cluster-conformance`**, exists where every other gear has two
//!   plus plugins. It is the only `*conformance*` crate in the repo. It is dev-dep-only
//!   and path-only, and it operationalizes `cpt-cf-clst-nfr-cross-backend-stability`:
//!   one shared test body run against every backend.
//! - **`wiring.rs` sits in the gear crate**, where the platform's other three live in
//!   `-sdk` crates. That is the §3.7 amendment above, not drift.
//!
//! Not a departure, though it reads like one: the private `domain` module holds no
//! `service.rs`, because invariant I14 forbids anything between the wire and the
//! backend. See that module's own docs for the rule that decides what belongs there —
//! it is a wire test, not the DDD reading of the word.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

pub mod api;
pub mod defaults;

mod config;
mod domain;
mod gear;

pub use config::{BackendBinding, ClusterConfig, DEFAULT_PROVIDER, ProfileConfig, SecretRef};
pub use domain::health::{ClusterReadiness, READINESS_PROBE_BUDGET};
pub use domain::local_client::LocalClusterClient;
pub use domain::provider::ProviderRegistry;
pub use domain::registry::{
    BoundProfile, InstanceId, ProfileInstanceRefs, ProfileRegistry, RegistrySnapshot,
};
pub use domain::wiring::{
    ClusterHandle, ClusterWiring, ClusterWiringBuilder, ProfileBackends, WiredCluster,
};

// Re-exported for convenience: plugins implement these from the SDK, but the
// config-driven wiring API surfaces them here too.
pub use cluster_sdk::{
    ClusterCacheProvider, ClusterLeaderElectionProvider, ClusterLockProvider, StopHook,
};
