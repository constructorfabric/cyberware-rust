//! Operator configuration (DESIGN.md §8).
//!
//! Four config shapes: the combined [`K8sClusterConfig`] and the three
//! per-primitive shapes ([`K8sCacheConfig`], [`K8sLeaderElectionConfig`],
//! [`K8sLockConfig`]) each provider deserializes. Each carries the shared subset
//! (`namespace`, `identity`, `lease_prefix`, `request_timeout_ms`,
//! `max_acquire_backoff_ms`, `skip_rbac_preflight`) plus only its own fields.
//!
//! ## Shared fields are duplicated, guarded by a test
//!
//! The shared subset is written out on each struct rather than pulled from one
//! inner struct via `#[serde(flatten)]`: serde's `flatten` is incompatible with
//! `#[serde(deny_unknown_fields)]` (it silently swallows unknown keys instead of
//! rejecting an operator typo), and the typo-rejection is worth more than avoiding
//! the duplication. This matches the shipped postgres plugin. The shared
//! `default_*` functions are centralized here so the defaults cannot drift, and
//! `tests::shared_subset_does_not_drift` asserts the four shapes deserialize the
//! shared keys identically.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Default `lease_prefix` (§2.2).
fn default_lease_prefix() -> String {
    "cluster".to_owned()
}
/// Default per-request timeout, ms (§4.2).
fn default_request_timeout() -> u64 {
    10_000
}
/// Default acquire-contention backoff ceiling, ms (§4.1).
fn default_max_acquire_backoff() -> u64 {
    5_000
}
/// Default election-TTL floor, ms (§2.10).
fn default_min_election_ttl() -> u64 {
    5_000
}
/// Default stale lock-object reaper interval, ms (§5.5).
fn default_reaper_interval() -> u64 {
    300_000
}
/// Default released-lock retention before reaping, ms (§5.5).
fn default_lock_object_retention() -> u64 {
    86_400_000
}
/// Default lock-name-cardinality WARN threshold (§5.5).
fn default_lock_name_cardinality_warn() -> u64 {
    1_000
}
/// Default `cache.watch: false` sweeper interval, ms (§6.2).
fn default_cache_sweep_interval() -> u64 {
    5_000
}
/// Default maximum cache value size, bytes (§6.6).
fn default_max_value_bytes() -> usize {
    262_144
}
/// Default bounded `put` retry budget (§6.1).
fn default_put_max_retries() -> u8 {
    3
}
/// `serde(default)` helper for the boolean fields that default to `true`.
fn default_true() -> bool {
    true
}

/// The cache read mode, and the consistency it declares (§6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReadMode {
    /// Quorum read through etcd; declares `Linearizable`. The default.
    #[default]
    Quorum,
    /// Watch-cache read (`resourceVersion=0`); declares `EventuallyConsistent`.
    Cached,
}

/// The combined config for [`K8sClusterPlugin`](crate::plugin) (all three
/// primitives, §3.2).
// The combined config carries independent on/off operator flags for three
// separate concerns (RBAC preflight, the lock reaper, the cache watcher); they are
// not a state better modelled as an enum.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct K8sClusterConfig {
    // ── shared subset ────────────────────────────────────────────────────────
    /// Namespace for every object; resolved from the downward API when omitted (§3.6).
    #[serde(default)]
    #[expand_vars]
    pub namespace: Option<String>,
    /// This instance's identity, written as `holderIdentity`; resolved when omitted (§3.6).
    #[serde(default)]
    #[expand_vars]
    pub identity: Option<String>,
    /// Prefix for every object name; RFC 1123 label, ≤ 40 (§2.2).
    #[serde(default = "default_lease_prefix")]
    pub lease_prefix: String,
    /// Per-request timeout, ms (§4.2).
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
    /// Ceiling on the jittered acquire-contention backoff, ms (§4.1).
    #[serde(default = "default_max_acquire_backoff")]
    pub max_acquire_backoff_ms: u64,
    /// Skip the `SelfSubjectAccessReview` RBAC probe (§3.4).
    #[serde(default)]
    pub skip_rbac_preflight: bool,

    // ── election-only ────────────────────────────────────────────────────────
    /// Floor on an election TTL, ms (§2.10).
    #[serde(default = "default_min_election_ttl")]
    pub min_election_ttl_ms: u64,
    /// Pin an election to a pre-existing Lease name — the migration escape hatch (§14).
    #[serde(default)]
    pub election_lease_names: BTreeMap<String, String>,

    // ── lock-only ────────────────────────────────────────────────────────────
    /// The stale lock-object reaper (§5.5).
    #[serde(default = "default_true")]
    pub reaper: bool,
    /// Reaper interval, ms (§5.5).
    #[serde(default = "default_reaper_interval")]
    pub reaper_interval_ms: u64,
    /// Released-lock retention before reaping, ms (§5.5).
    #[serde(default = "default_lock_object_retention")]
    pub lock_object_retention_ms: u64,
    /// WARN past this many distinct lock names (§5.5).
    #[serde(default = "default_lock_name_cardinality_warn")]
    pub lock_name_cardinality_warn_threshold: u64,

    // ── cache-only ───────────────────────────────────────────────────────────
    /// Read mode (§6.5).
    #[serde(default)]
    pub cache_reads: ReadMode,
    /// Whether the cache maintains its shared watcher (§6.3).
    #[serde(default = "default_true")]
    pub cache_watch: bool,
    /// Sweeper interval used only when `cache_watch: false`, ms (§6.2).
    #[serde(default = "default_cache_sweep_interval")]
    pub cache_sweep_interval_ms: u64,
    /// Max raw cache value bytes; rejected locally before a request (§6.6).
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,
    /// Bounded retry budget for an unconditional `put` losing a race (§6.1).
    #[serde(default = "default_put_max_retries")]
    pub put_max_retries: u8,
}

/// The standalone leader-election config (§3.5).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct K8sLeaderElectionConfig {
    /// See [`K8sClusterConfig::namespace`].
    #[serde(default)]
    #[expand_vars]
    pub namespace: Option<String>,
    /// See [`K8sClusterConfig::identity`].
    #[serde(default)]
    #[expand_vars]
    pub identity: Option<String>,
    /// See [`K8sClusterConfig::lease_prefix`].
    #[serde(default = "default_lease_prefix")]
    pub lease_prefix: String,
    /// See [`K8sClusterConfig::request_timeout_ms`].
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
    /// See [`K8sClusterConfig::max_acquire_backoff_ms`].
    #[serde(default = "default_max_acquire_backoff")]
    pub max_acquire_backoff_ms: u64,
    /// See [`K8sClusterConfig::skip_rbac_preflight`].
    #[serde(default)]
    pub skip_rbac_preflight: bool,
    /// See [`K8sClusterConfig::min_election_ttl_ms`].
    #[serde(default = "default_min_election_ttl")]
    pub min_election_ttl_ms: u64,
    /// See [`K8sClusterConfig::election_lease_names`].
    #[serde(default)]
    pub election_lease_names: BTreeMap<String, String>,
}

/// The standalone distributed-lock config (§3.5).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct K8sLockConfig {
    /// See [`K8sClusterConfig::namespace`].
    #[serde(default)]
    #[expand_vars]
    pub namespace: Option<String>,
    /// See [`K8sClusterConfig::identity`].
    #[serde(default)]
    #[expand_vars]
    pub identity: Option<String>,
    /// See [`K8sClusterConfig::lease_prefix`].
    #[serde(default = "default_lease_prefix")]
    pub lease_prefix: String,
    /// See [`K8sClusterConfig::request_timeout_ms`].
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
    /// See [`K8sClusterConfig::max_acquire_backoff_ms`].
    #[serde(default = "default_max_acquire_backoff")]
    pub max_acquire_backoff_ms: u64,
    /// See [`K8sClusterConfig::skip_rbac_preflight`].
    #[serde(default)]
    pub skip_rbac_preflight: bool,
    /// See [`K8sClusterConfig::reaper`].
    #[serde(default = "default_true")]
    pub reaper: bool,
    /// See [`K8sClusterConfig::reaper_interval_ms`].
    #[serde(default = "default_reaper_interval")]
    pub reaper_interval_ms: u64,
    /// See [`K8sClusterConfig::lock_object_retention_ms`].
    #[serde(default = "default_lock_object_retention")]
    pub lock_object_retention_ms: u64,
    /// See [`K8sClusterConfig::lock_name_cardinality_warn_threshold`].
    #[serde(default = "default_lock_name_cardinality_warn")]
    pub lock_name_cardinality_warn_threshold: u64,
}

/// The standalone cache config (§3.5).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct K8sCacheConfig {
    /// See [`K8sClusterConfig::namespace`].
    #[serde(default)]
    #[expand_vars]
    pub namespace: Option<String>,
    /// See [`K8sClusterConfig::identity`].
    #[serde(default)]
    #[expand_vars]
    pub identity: Option<String>,
    /// See [`K8sClusterConfig::lease_prefix`].
    #[serde(default = "default_lease_prefix")]
    pub lease_prefix: String,
    /// See [`K8sClusterConfig::request_timeout_ms`].
    #[serde(default = "default_request_timeout")]
    pub request_timeout_ms: u64,
    /// See [`K8sClusterConfig::max_acquire_backoff_ms`].
    #[serde(default = "default_max_acquire_backoff")]
    pub max_acquire_backoff_ms: u64,
    /// See [`K8sClusterConfig::skip_rbac_preflight`].
    #[serde(default)]
    pub skip_rbac_preflight: bool,
    /// See [`K8sClusterConfig::cache_reads`].
    #[serde(default)]
    pub cache_reads: ReadMode,
    /// See [`K8sClusterConfig::cache_watch`].
    #[serde(default = "default_true")]
    pub cache_watch: bool,
    /// See [`K8sClusterConfig::cache_sweep_interval_ms`].
    #[serde(default = "default_cache_sweep_interval")]
    pub cache_sweep_interval_ms: u64,
    /// See [`K8sClusterConfig::max_value_bytes`].
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,
    /// See [`K8sClusterConfig::put_max_retries`].
    #[serde(default = "default_put_max_retries")]
    pub put_max_retries: u8,
}

#[cfg(test)]
mod tests {
    use super::{
        K8sCacheConfig, K8sClusterConfig, K8sLeaderElectionConfig, K8sLockConfig, ReadMode,
    };
    use serde_json::json;
    use toolkit::var_expand::ExpandVars;

    #[test]
    fn defaults() {
        let c: K8sClusterConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.lease_prefix, "cluster");
        assert_eq!(c.request_timeout_ms, 10_000);
        assert_eq!(c.max_acquire_backoff_ms, 5_000);
        assert!(!c.skip_rbac_preflight);
        assert_eq!(c.min_election_ttl_ms, 5_000);
        assert!(c.reaper);
        assert_eq!(c.reaper_interval_ms, 300_000);
        assert_eq!(c.lock_object_retention_ms, 86_400_000);
        assert_eq!(c.lock_name_cardinality_warn_threshold, 1_000);
        assert_eq!(c.cache_reads, ReadMode::Quorum);
        assert!(c.cache_watch);
        assert_eq!(c.cache_sweep_interval_ms, 5_000);
        assert_eq!(c.max_value_bytes, 262_144);
        assert_eq!(c.put_max_retries, 3);
        assert!(c.namespace.is_none() && c.identity.is_none());
        assert!(c.election_lease_names.is_empty());
    }

    #[test]
    fn deny_unknown_fields_rejects_a_typo() {
        // `namespaec` is a typo for `namespace`.
        let err = serde_json::from_value::<K8sClusterConfig>(json!({ "namespaec": "gears" }));
        assert!(err.is_err(), "a typo'd field must be rejected");
    }

    #[test]
    fn read_mode_rejects_unknown_variant() {
        assert!(serde_json::from_value::<ReadMode>(json!("stale")).is_err());
        assert_eq!(
            serde_json::from_value::<ReadMode>(json!("cached")).unwrap(),
            ReadMode::Cached
        );
    }

    #[test]
    fn expand_vars_on_namespace_and_identity() {
        temp_env::with_vars(
            [("K8S_NS", Some("gears-prod")), ("K8S_ID", Some("broker-7"))],
            || {
                let mut c: K8sClusterConfig = serde_json::from_value(json!({
                    "namespace": "${K8S_NS}",
                    "identity": "${K8S_ID:-fallback}",
                }))
                .unwrap();
                c.expand_vars().unwrap();
                assert_eq!(c.namespace.as_deref(), Some("gears-prod"));
                assert_eq!(c.identity.as_deref(), Some("broker-7"));
            },
        );
    }

    #[test]
    fn expand_vars_missing_var_errors() {
        let mut c: K8sClusterConfig =
            serde_json::from_value(json!({ "namespace": "${DEFINITELY_UNSET_VAR}" })).unwrap();
        assert!(c.expand_vars().is_err());
    }

    /// The drift guard: a JSON carrying every shared-subset key must deserialize
    /// into all four config shapes with identical shared values. A shared field
    /// dropped from one struct makes `deny_unknown_fields` reject this payload
    /// there; a renamed one changes the read value — either way this fails.
    #[test]
    fn shared_subset_does_not_drift() {
        let shared = json!({
            "namespace": "gears",
            "identity": "broker-7",
            "lease_prefix": "cf",
            "request_timeout_ms": 7_000_u64,
            "max_acquire_backoff_ms": 2_000_u64,
            "skip_rbac_preflight": true,
        });
        let combined: K8sClusterConfig = serde_json::from_value(shared.clone()).unwrap();
        let leader: K8sLeaderElectionConfig = serde_json::from_value(shared.clone()).unwrap();
        let lock: K8sLockConfig = serde_json::from_value(shared.clone()).unwrap();
        let cache: K8sCacheConfig = serde_json::from_value(shared).unwrap();

        // Namespace / identity.
        for ns in [
            &combined.namespace,
            &leader.namespace,
            &lock.namespace,
            &cache.namespace,
        ] {
            assert_eq!(ns.as_deref(), Some("gears"));
        }
        // The remaining shared scalars, compared against the combined config.
        assert_eq!(leader.lease_prefix, combined.lease_prefix);
        assert_eq!(lock.lease_prefix, combined.lease_prefix);
        assert_eq!(cache.lease_prefix, combined.lease_prefix);
        assert_eq!(leader.request_timeout_ms, combined.request_timeout_ms);
        assert_eq!(lock.request_timeout_ms, combined.request_timeout_ms);
        assert_eq!(cache.request_timeout_ms, combined.request_timeout_ms);
        assert_eq!(
            leader.max_acquire_backoff_ms,
            combined.max_acquire_backoff_ms
        );
        assert_eq!(lock.max_acquire_backoff_ms, combined.max_acquire_backoff_ms);
        assert_eq!(
            cache.max_acquire_backoff_ms,
            combined.max_acquire_backoff_ms
        );
        assert!(
            leader.skip_rbac_preflight
                && lock.skip_rbac_preflight
                && cache.skip_rbac_preflight
                && combined.skip_rbac_preflight
        );
    }
}
