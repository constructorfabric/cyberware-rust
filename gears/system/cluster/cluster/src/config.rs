//! Operator YAML schema for the cluster gear (DESIGN §3.4 / §3.11).
//!
//! [`ClusterConfig`] is the operator-facing contract: a map of named profiles,
//! each binding the three coordination primitives to a backend `provider`. The
//! `cache` binding is the required anchor; the other three may be omitted to ride
//! the SDK default backends over that profile's cache
//! (`cpt-cf-clst-fr-routing-omit-default`), or bound to their own provider for
//! per-primitive routing (`cpt-cf-clst-fr-routing-per-primitive`).
//!
//! These types are serde-deserializable (typically via `ctx.config()` in a host
//! gear, fed by `serde-saphyr`). They live in the wiring crate, not the SDK — the
//! SDK coordination contract stays serde-free per `cpt-cf-clst-constraint-no-serde`.
//!
//! Per-provider options are **flattened** into the backend binding and parsed by
//! the provider itself (see [`crate::domain::provider::ClusterCacheProvider`]), so adding
//! a backend is a new crate plus config, not a schema change here.

use serde::Deserialize;

/// The reserved [`BackendBinding::provider`] name meaning "the SDK default backend
/// over this profile's cache" — the omit-default behaviour, but as an *explicit*
/// binding that can carry options.
///
/// Omitting a primitive entirely and binding it to `default` resolve to the same
/// backend; the difference is that a binding has somewhere to put options.
///
/// The only such option today is `allow_weak_consistency`, which a profile over an
/// eventually-consistent cache needs in order to start at all: both CAS-based SDK
/// defaults reject such a cache, so without the opt-in a profile binding
/// `cache: { provider: redis }` and omitting `leader_election` or `lock` fails
/// startup (ADR-009). The option is accepted on `leader_election` and `lock`, whose
/// defaults have that consistency guard. It defaults to `false`, so the loud startup
/// failure stays the default behaviour, and it does **not** launder capability
/// validation — a consumer
/// requiring [`CacheCapability::Linearizable`](cluster_sdk::CacheCapability::Linearizable)
/// against the same profile still fails startup regardless.
///
/// It is a reserved name, not a registry entry: `ClusterWiring::from_config`
/// intercepts it *before* any [`ProviderRegistry`](crate::ProviderRegistry)
/// lookup, so a plugin that registered a provider literally called `default` would
/// never be reached. It is rejected outright on the `cache` binding — the cache is
/// the anchor the defaults wrap, so there is no default cache to resolve to.
pub const DEFAULT_PROVIDER: &str = "default";

/// The whole cluster section of operator YAML: a set of named profiles.
///
/// ```yaml
/// cluster:
///   profiles:
///     default:
///       cache: { provider: standalone }
///       # leader_election / lock omitted → SDK defaults
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// Profile name → per-primitive backend bindings. Profile names must conform
    /// to the cluster name rule (`[a-zA-Z0-9_-]+`); the wiring validates this at
    /// registration time.
    #[serde(default)]
    pub profiles: std::collections::BTreeMap<String, ProfileConfig>,

    /// How long a lease record outlives the lease it fenced, so the fence stays
    /// monotonic across a lapse (DESIGN.md). Written the way
    /// every other duration in platform config is — `1h`, `30m`, `90s`.
    ///
    /// Defaults to
    /// [`FENCE_RETENTION_DEFAULT`](cluster_sdk::lease::FENCE_RETENTION_DEFAULT)
    /// (an hour). Zero is rejected at startup; a value below the longest lease
    /// TTL in use warns at acquisition, since that TTL is a per-call argument
    /// rather than anything this file could compare against
    /// (`cluster_sdk::lease::validate_fence_retention`).
    ///
    /// # What it does and does not reach
    ///
    /// It governs the **cache-backed default backends** — the lock and leader
    /// election a profile gets by omitting those primitives — because those are
    /// the ones whose fence lives in a cache value this crate writes. A *native*
    /// backend holding its own fence in its own columns takes its own option:
    /// the Postgres lock's `fence_retention` sits in that binding's provider
    /// options, beside its DSN.
    ///
    /// That split is deliberate, and it is the alternative to injecting this key
    /// into every provider's option map — which would make it a silent addition
    /// to the plugin contract that any `deny_unknown_fields` provider config
    /// would reject. Two windows cannot disagree in a way that matters: a lease
    /// name lives in exactly one backend, and the guarantee is stated per lease
    /// name.
    #[serde(default, with = "toolkit_utils::humantime_serde::option")]
    pub fence_retention: Option<std::time::Duration>,
}

impl ClusterConfig {
    /// The retention window to apply, defaulted and validated.
    ///
    /// # Errors
    /// [`ClusterError::InvalidConfig`](cluster_sdk::error::ClusterError::InvalidConfig)
    /// when the operator set a zero window.
    pub fn fence_retention(&self) -> Result<std::time::Duration, cluster_sdk::ClusterError> {
        let retention = self
            .fence_retention
            .unwrap_or(cluster_sdk::lease::FENCE_RETENTION_DEFAULT);
        cluster_sdk::lease::validate_fence_retention(retention)?;
        Ok(retention)
    }
}

/// The per-primitive backend bindings for one profile.
///
/// `cache` is required (it is the omit-default anchor). Each of the other two
/// primitives may be bound to its own provider or omitted; an omitted primitive
/// is auto-filled with the SDK default backend over this profile's cache.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// The cache backend — required. Serves as the anchor the SDK default
    /// leader-election and lock backends wrap when those primitives are omitted.
    pub cache: BackendBinding,
    /// An explicit leader-election backend. Omit to use the SDK default over the
    /// cache.
    #[serde(default)]
    pub leader_election: Option<BackendBinding>,
    /// An explicit distributed-lock backend. Omit to use the SDK default over the
    /// cache.
    #[serde(default)]
    pub lock: Option<BackendBinding>,
}

/// One primitive's binding to a backend `provider`, plus that provider's own
/// options (flattened) and an optional credential reference.
///
/// The known keys are `provider` and `secret_ref`; every other key is captured
/// into [`options`](Self::options) verbatim for the provider to parse. This keeps
/// the schema open: a new backend defines its own option keys without changing
/// this struct.
#[derive(Debug, Clone, Deserialize)]
pub struct BackendBinding {
    /// The backend provider name, e.g. `standalone`, `postgres`, `redis`,
    /// `k8s-lease`. Matched against the registered providers at wiring time; an
    /// unknown provider fails startup with `ClusterError::InvalidConfig`.
    ///
    /// [`DEFAULT_PROVIDER`] (`default`) is reserved: on `leader_election` or `lock`
    /// it selects the SDK default backend over this
    /// profile's cache — the same backend omitting the primitive would produce,
    /// but as a binding that can carry `allow_weak_consistency`. It is rejected
    /// on `cache`.
    pub provider: String,
    /// A provisional, OPEN reference to the credential the backend uses to reach
    /// its infrastructure (DESIGN §3 open question — credential wiring is deferred
    /// to the OOP deployment design). Placeholder shape only; not a committed
    /// contract. Ignored by the in-process standalone provider.
    #[serde(default)]
    pub secret_ref: Option<SecretRef>,
    /// Provider-specific options captured verbatim (Design A: flattened options).
    /// The provider deserializes the keys it understands from this map.
    #[serde(flatten)]
    pub options: serde_json::Map<String, serde_json::Value>,
}

/// Provisional placeholder for a backend credential reference (DESIGN §3 open
/// question, deferred to the OOP deployment design). The concrete resolution —
/// credstore lookup, K8s service-account fallback, rotation — is intentionally
/// unspecified here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
    /// An opaque name the future credential layer will resolve. Treated as an
    /// opaque string for now.
    pub name: String,
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
