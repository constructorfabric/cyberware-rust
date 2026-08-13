use serde::Deserialize;
use toolkit::var_expand::{ExpandVars as ExpandVarsTrait, ExpandVarsError};

/// Wrapper around a config string whose final value is a secret.
///
/// Holds a plain `String` for `Deserialize` + `ExpandVars` compatibility
/// (toolkit's `#[expand_vars]` derive substitutes `${VAR}` placeholders on
/// `String` fields; `secrecy::SecretString` is not `ExpandVars`-aware), while
/// suppressing every accidental leak surface:
///
/// * No `Display` impl — `format!("{secret}")` won't compile.
/// * `Debug` emits `<redacted>`, so `tracing::debug!(?cfg)` / panic-formatter
///   dumps never print the resolved `ClickHouse` URL (which embeds credentials).
/// * No `Serialize`, no `PartialEq` — secret bytes never leak through a
///   config-snapshot path or an assertion message.
///
/// The only read accessor is [`Self::expose`] (deliberately verbose so every
/// read site is grep-able). `expand_vars` runs on the inner `String` before
/// any consumer sees the value.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct SecretFromEnv(String);

impl SecretFromEnv {
    /// Construct directly from an already-resolved value, skipping `${VAR}`
    /// expansion. Config deserialization goes through `#[serde(transparent)]`
    /// instead; this is for call sites (tests, in-process fixtures) that
    /// already hold a resolved secret string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read the resolved secret bytes. Use only at boundaries that consume the
    /// URL (config validation, building the `ClickHouse` client); never log the
    /// returned value.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretFromEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl ExpandVarsTrait for SecretFromEnv {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.0.expand_vars()
    }
}

/// Return `true` when `url` uses a plaintext `http://` connection (no TLS).
///
/// Used by [`ClickHousePluginConfig::validate`] to fail closed on an
/// unencrypted `database_url` by default, and by
/// [`crate::infra::storage::pool::build_client`] for the runtime TLS-posture
/// warning. `pub(crate)` so both call sites — and their unit tests — share
/// one definition instead of duplicating the scheme check.
pub(crate) fn is_plaintext_url(url: &str) -> bool {
    url.starts_with("http://")
}

/// Configuration for the `ClickHouse` Usage Collector storage backend.
/// Durations are whole seconds (repo convention).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClickHousePluginConfig {
    /// `ClickHouse` HTTP endpoint URL including credentials, e.g.
    /// `https://user:${CH_PASSWORD}@host:8443/db`. Wrapped in [`SecretFromEnv`]
    /// (Debug-redacted, no Display / Serialize); `${VAR}` templating is
    /// expanded via the `#[expand_vars]` derive. [`Self::validate`] rejects a
    /// plaintext `http://` URL unless [`Self::allow_insecure_http`] is set.
    #[expand_vars]
    pub database_url: SecretFromEnv,
    /// Explicit development/test opt-out for a plaintext (`http://`)
    /// `database_url`. `database_url` embeds credentials
    /// ([`Self::database_url`]), so an unencrypted connection sends them —
    /// and every usage record — over the network in cleartext. Defaults to
    /// `false`: [`Self::validate`] rejects a `http://` `database_url` unless
    /// this is explicitly set to `true`. Has no effect on a `https://` URL.
    pub allow_insecure_http: bool,
    /// HTTP request timeout in seconds (applies to reads and writes).
    pub request_timeout_secs: u64,
    /// Cluster distributed-lock lease TTL in seconds. Must exceed worst-case
    /// create/delete critical-section latency (`ClickHouse` round-trips while the
    /// lock is held). Renewed immediately before the mutating write.
    pub lock_ttl_secs: u64,
    /// Maximum time to wait when acquiring the per-`gts_id` exclusive cluster
    /// lock. On timeout the operation fails closed with `Transient`.
    pub lock_timeout_secs: u64,
    /// `usage_records` retention window in seconds; rows older are deleted via
    /// `ClickHouse` TTL. Must be in `(0, MAX_RETENTION_SECS]`.
    pub retention_period_secs: u64,
    /// Vendor name for GTS instance registration.
    pub vendor: String,
    /// Plugin priority (lower = higher priority).
    pub priority: i16,
}

impl Default for ClickHousePluginConfig {
    fn default() -> Self {
        Self {
            database_url: SecretFromEnv::default(),
            allow_insecure_http: false,
            request_timeout_secs: 30,
            lock_ttl_secs: 30,
            lock_timeout_secs: 5,
            retention_period_secs: 365 * 86_400, // 365 days
            vendor: "cyberfabric".to_owned(),
            priority: 10,
        }
    }
}

/// Upper bound on `retention_period_secs` (100 years in seconds).
///
/// `ClickHouse`'s TTL interval expression is evaluated as a `DateTime` offset;
/// a pathological retention window would overflow the `DateTime` type and
/// surface as a confusing DDL failure at schema-provisioning time. 100 years
/// is far beyond any realistic usage-data retention while staying safely inside
/// `ClickHouse`'s `DateTime64` range.
const MAX_RETENTION_SECS: u64 = 100 * 365 * 86_400;

impl ClickHousePluginConfig {
    /// Validate invariants not expressible in the type system.
    ///
    /// # Errors
    ///
    /// Returns an error string for an empty `database_url`, a plaintext
    /// `http://` `database_url` without [`Self::allow_insecure_http`], a
    /// zero timeout or lock TTL, a retention window outside
    /// `(0, MAX_RETENTION_SECS]`, or a blank `vendor`.
    pub fn validate(&self) -> Result<(), String> {
        if self.database_url.expose().trim().is_empty() {
            return Err("database_url must not be empty".to_owned());
        }
        if let Err(e) = url::Url::parse(self.database_url.expose()) {
            return Err(format!("database_url must be a valid absolute URL: {e}"));
        }
        if self.request_timeout_secs == 0 {
            return Err("request_timeout_secs must be > 0".to_owned());
        }
        if self.lock_ttl_secs == 0 {
            return Err("lock_ttl_secs must be > 0".to_owned());
        }
        if self.lock_timeout_secs == 0 {
            return Err("lock_timeout_secs must be > 0".to_owned());
        }
        if self.retention_period_secs == 0 {
            return Err("retention_period_secs must be > 0".to_owned());
        }
        if self.retention_period_secs > MAX_RETENTION_SECS {
            return Err(format!(
                "retention_period_secs must be <= {MAX_RETENTION_SECS} (100 years); \
                 a larger window overflows the ClickHouse DateTime64 TTL expression"
            ));
        }
        if self.vendor.trim().is_empty() {
            return Err(
                "vendor must not be empty; it is part of the GTS instance identity registered \
                 with types-registry (e.g. vendor = \"cyberfabric\")"
                    .to_owned(),
            );
        }
        // Fail closed on a plaintext database_url: it embeds credentials and
        // carries every usage record, so an unencrypted connection is a
        // credential- and data-exposure risk, not just a style choice. The
        // override must be set explicitly and is not implied by any other
        // field (e.g. a `http://` scheme alone is never sufficient consent).
        if is_plaintext_url(self.database_url.expose()) && !self.allow_insecure_http {
            return Err(
                "database_url uses a plaintext http:// scheme, which sends credentials and \
                 usage data unencrypted; use https:// or set allow_insecure_http = true to \
                 explicitly opt out for local development/test only"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
