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

/// Return `true` when `url` uses a plaintext `http` connection (no TLS).
///
/// Used by [`ClickHousePluginConfig::validate`] to fail closed on an
/// unencrypted `database_url` by default, and by
/// [`crate::infra::storage::pool::build_client`] for the runtime TLS-posture
/// warning. `pub(crate)` so both call sites — and their unit tests — share
/// one definition instead of duplicating the scheme check.
///
/// The scheme is compared after URL parsing, not as a string prefix: URL
/// schemes are case-insensitive and `Url::parse` normalizes them to lowercase,
/// so `HTTP://host/db` is a cleartext connection that a `starts_with("http://")`
/// test would wave through — bypassing the gate and the warning both.
///
/// Input that does not parse as an absolute URL is reported as plaintext: it
/// cannot be shown to be encrypted, and this is the fail-closed direction.
/// `validate` rejects unparseable input earlier, so in practice this only
/// affects `build_client`'s defense-in-depth warning, where a spurious warning
/// is harmless and a missed one is not.
pub(crate) fn is_plaintext_url(url: &str) -> bool {
    match url::Url::parse(url) {
        Ok(parsed) => parsed.scheme() == "http",
        Err(_) => true,
    }
}

/// Configuration for the `ClickHouse` Usage Collector storage backend.
/// Durations are whole seconds (repo convention).
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClickHousePluginConfig {
    /// `ClickHouse` HTTP endpoint URL including credentials, e.g.
    /// `https://user:${CH_PASSWORD}@host:8443/db`. Wrapped in [`SecretFromEnv`]
    /// (Debug-redacted, no Display / Serialize); `${VAR}` templating is
    /// expanded via the `#[expand_vars]` derive. [`Self::validate`] admits only
    /// the `http` and `https` schemes, and rejects a plaintext `http` URL
    /// unless [`Self::allow_insecure_http`] is set. Both checks read the parsed
    /// (lowercase-normalized) scheme, so `HTTP://` is treated as `http://`.
    #[expand_vars]
    pub database_url: SecretFromEnv,
    /// Explicit development/test opt-out for a plaintext (`http://`)
    /// `database_url`. `database_url` embeds credentials
    /// ([`Self::database_url`]), so an unencrypted connection sends them —
    /// and every usage record — over the network in cleartext. Defaults to
    /// `false`: [`Self::validate`] rejects a `http://` `database_url` unless
    /// this is explicitly set to `true`. Has no effect on a `https://` URL, and
    /// does not admit a scheme outside `http`/`https`: it is consent to skip
    /// TLS, not consent to a scheme the `ClickHouse` client cannot speak.
    pub allow_insecure_http: bool,
    /// HTTP request timeout in seconds (applies to reads and writes).
    ///
    /// Drives two distinct mechanisms: the `ClickHouse` server settings
    /// `send_timeout` / `receive_timeout`, and — `CLIENT_DEADLINE_GRACE_SECS`
    /// later — the client-side deadline from `Self::client_deadline`.
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
            // Same window the migration DDL bakes in, so a config-less start
            // needs no `MODIFY TTL` reconciliation at startup.
            retention_period_secs: crate::infra::storage::pool::DEFAULT_RETENTION_SECS,
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

/// Grace added to the server-side timeout to form the client-side deadline
/// (see [`ClickHousePluginConfig::client_deadline`]).
pub(crate) const CLIENT_DEADLINE_GRACE_SECS: u64 = 5;

impl ClickHousePluginConfig {
    /// Client-side deadline for a single `ClickHouse` request.
    ///
    /// [`Self::request_timeout_secs`] is forwarded to `ClickHouse` as the
    /// server settings `send_timeout` / `receive_timeout`, which the *server*
    /// applies to its own socket handling. They therefore do nothing when a
    /// connection is accepted and then never answered, or when an intermediary
    /// holds the socket open — cases where a request would otherwise hang
    /// indefinitely. This deadline is the client-side backstop for exactly
    /// those cases.
    ///
    /// It sits [`CLIENT_DEADLINE_GRACE_SECS`] *past* the server-side budget so
    /// that when the server is answering, its own timeout fires first and the
    /// caller gets the server's descriptive error rather than a bare
    /// client-side timeout.
    pub(crate) fn client_deadline(&self) -> std::time::Duration {
        // `validate` bounds `request_timeout_secs` only from below (> 0), so a
        // pathologically large configured value must not overflow the add.
        std::time::Duration::from_secs(
            self.request_timeout_secs
                .saturating_add(CLIENT_DEADLINE_GRACE_SECS),
        )
    }
    /// Validate invariants not expressible in the type system.
    ///
    /// # Errors
    ///
    /// Returns an error string for an empty `database_url`, one whose scheme is
    /// neither `http` nor `https`, a plaintext `http` `database_url` without
    /// [`Self::allow_insecure_http`], a zero timeout or lock TTL, a retention
    /// window outside `(0, MAX_RETENTION_SECS]`, or a blank `vendor`.
    pub fn validate(&self) -> Result<(), String> {
        if self.database_url.expose().trim().is_empty() {
            return Err("database_url must not be empty".to_owned());
        }
        let parsed = match url::Url::parse(self.database_url.expose()) {
            Ok(parsed) => parsed,
            Err(e) => return Err(format!("database_url must be a valid absolute URL: {e}")),
        };
        // The `clickhouse` crate speaks only ClickHouse's HTTP interface, so a
        // native-protocol or non-network scheme is a misconfiguration that would
        // otherwise surface as an opaque request failure on the first query.
        // Only the scheme is interpolated: `database_url` embeds credentials.
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "database_url scheme `{}` is not supported; the ClickHouse client speaks the \
                 HTTP interface only, so database_url must use https:// (or http:// with \
                 allow_insecure_http = true for local development/test)",
                parsed.scheme()
            ));
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
