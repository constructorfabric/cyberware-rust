//! Configuration gear for toolkit-bootstrap
//!
//! This gear provides configuration types and utilities for both host and `OoP` gears.

mod dump;

use anyhow::{Context, Result, ensure};
// Use DB config types from toolkit-db
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
pub use toolkit_db::{DbConnConfig, GlobalDatabaseConfig, PoolCfg};
use tracing::Level;

use crate::ConfigProvider;
use crate::telemetry::OpenTelemetryConfig;
use url::Url;

/// Normalize a path to use forward slashes (for cross-platform YAML/DSN compatibility).
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Error type for vendor configuration access.
#[derive(thiserror::Error, Debug)]
pub enum VendorConfigError {
    #[error("vendor '{vendor}' not found in configuration")]
    NotFound { vendor: String },
    // Intentionally not named `source`; doing so would duplicate chained error output.
    #[error("invalid config for vendor '{vendor}': {cause}")]
    InvalidConfig {
        vendor: String,
        cause: serde_json::Error,
    },
}

// Re-export dump functions
pub use dump::{
    dump_effective_gears_config_json, dump_effective_gears_config_yaml, list_gear_names,
    redact_dsn_password, render_effective_gears_config,
};

/// Small typed view to parse each gear entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GearConfig {
    #[serde(default)]
    pub database: Option<DbConnConfig>,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub runtime: Option<GearRuntime>,
    #[serde(default)] // Used by the CLI
    pub metadata: serde_json::Value,
}

/// Runtime configuration for a gear (local vs out-of-process).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GearRuntime {
    #[serde(default, rename = "type")]
    pub mod_type: RuntimeKind,
    /// Execution configuration for `OoP` gears.
    #[serde(default)]
    pub execution: Option<ExecutionConfig>,
}

/// Execution configuration for out-of-process gears.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    /// Path to the executable. Supports absolute paths or `~` expansion.
    pub executable_path: String,
    /// Command-line arguments to pass to the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the process (optional, defaults to current dir).
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Environment variables to set for the process.
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

/// Gear runtime kind.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Local,
    Oop,
}

/// Main application configuration with strongly-typed global sections
/// and a flexible per-gear configuration bag.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Core server configuration.
    pub server: ServerConfig,
    /// New typed database configuration (optional).
    pub database: Option<GlobalDatabaseConfig>,
    /// Logging configuration
    #[serde(default = "default_logging_config")]
    pub logging: LoggingConfig,
    /// OpenTelemetry configuration (resource, tracing, metrics).
    #[serde(default)]
    pub opentelemetry: OpenTelemetryConfig,
    /// Directory containing per-gear YAML files (optional).
    #[serde(default)]
    pub gears_dir: Option<String>,
    /// Per-gear configuration bag: `gear_name` → arbitrary JSON/YAML value.
    #[serde(default)]
    pub gears: HashMap<String, serde_json::Value>,
    /// Per-vendor configuration bag: `vendor_name` → arbitrary JSON/YAML value.
    /// Allows vendors to add their own typed configuration sections.
    #[serde(default)]
    pub vendor: VendorConfig,
    /// Out-of-process HTTP server configuration.
    ///
    /// When present, an `OoP` gear starts an Axum HTTP server (probes,
    /// gear routes, self-registration, dependency resolution, graceful drain)
    /// instead of the legacy gRPC-only lifecycle (`cpt-cf-component-oop-bootstrap`).
    #[serde(default)]
    pub oop_http: Option<OopHttpConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        let server = ServerConfig::default();
        Self {
            server,
            database: None,
            logging: default_logging_config(),
            opentelemetry: OpenTelemetryConfig::default(),
            gears_dir: None,
            gears: HashMap::new(),
            vendor: VendorConfig::new(),
            oop_http: None,
        }
    }
}

/// Out-of-process HTTP server configuration (`cpt-cf-component-oop-bootstrap`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OopHttpConfig {
    /// Address the main HTTP server binds to (gear routes + probes),
    /// e.g. `"0.0.0.0:8080"`.
    pub listen_addr: String,
    /// Optional separate bind address for probe endpoints (sidecar port).
    /// When set, `/healthz` and `/readyz` are also served here.
    #[serde(default)]
    pub probe_bind_addr: Option<String>,
    /// Maximum seconds to wait for in-flight requests to drain on shutdown.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
    /// Per-check timeout (ms) for readiness healthchecks on `/readyz`; raise for
    /// slow dependencies. Mirrors the `api-gateway` `healthcheck_timeout_ms`.
    #[serde(default = "default_healthcheck_timeout_ms")]
    pub healthcheck_timeout_ms: u64,
    /// Base URL other services use to reach this instance (registered as the
    /// instance's REST endpoint). Defaults to `http://<listen_addr>` with an
    /// unspecified host (`0.0.0.0`) rewritten to `127.0.0.1`.
    #[serde(default)]
    pub advertise_uri: Option<String>,
    /// Allow a loopback / unspecified `advertise_uri` (`127.0.0.1`, `::1`,
    /// `localhost`, `0.0.0.0`, `[::]`). Off by default: such an endpoint is
    /// registered-but-unreachable in multi-host Profile 2 / Profile 3, so
    /// bootstrap fails fast (`cpt-cf-adr-instance-addressable-discovery`).
    /// Set `true` only for single-host / local-dev.
    #[serde(default)]
    pub allow_loopback_advertise: bool,
    /// Platform-plane (`InternalAuthenticator`) configuration. When present it
    /// drives both the *inbound* HTTP validator on the gear's own routes and
    /// the *outbound* credential attached to the gear's `DirectoryService`
    /// calls. The `shared_secret` provider works out of the box; the `kube`
    /// provider's inbound `TokenReview` validator requires the `k8s-auth`
    /// feature.
    #[serde(default)]
    pub internal_auth: Option<toolkit_security::InternalAuthConfig>,
    /// Stable addressing labels (k8s `matchLabels` style) advertised with this
    /// instance's directory registration, for label-based instance selection
    /// (`DirectoryClient::resolve_by_labels`).
    ///
    /// Sourced from config (`oop_http.labels.<key>`) or the environment
    /// (`APP__OOP_HTTP__LABELS__<KEY>`). A bare-numeric env *value* (e.g. a
    /// `StatefulSet` ordinal injected as `APP__OOP_HTTP__LABELS__SHARD=7`) is
    /// coerced to a string here rather than aborting the config load. Note:
    /// environment-sourced keys are still lower-cased by the config loader, so
    /// keys that must preserve case or contain `.`/`-` should be set in the
    /// config file rather than via env.
    #[serde(default, deserialize_with = "de_labels_scalar_to_string")]
    pub labels: std::collections::BTreeMap<String, String>,
}

fn default_drain_timeout_secs() -> u64 {
    30
}

fn default_healthcheck_timeout_ms() -> u64 {
    500
}

/// Deserialize a label map, coercing scalar values (numbers, booleans) to
/// strings.
///
/// The environment layer parses a bare-numeric value like
/// `APP__OOP_HTTP__LABELS__SHARD=7` into an integer, which would otherwise fail
/// to deserialize into a `String` and abort the entire `AppConfig::load_layered`
/// — precisely on the path a k8s `StatefulSet` ordinal is injected. Accepting the
/// scalar and rendering it as a string keeps that value load-able as a label.
fn de_labels_scalar_to_string<'de, D>(
    deserializer: D,
) -> Result<std::collections::BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;

    // Ordering matters for `untagged`: a string input matches `Str`; an integer
    // matches `I64`/`U64` before `F64`, so `7` renders as `"7"` not `"7.0"`.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Scalar {
        Str(String),
        Bool(bool),
        I64(i64),
        U64(u64),
        F64(f64),
    }

    let raw = std::collections::BTreeMap::<String, Scalar>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|(k, v)| {
            let value = match v {
                Scalar::Str(s) => s,
                Scalar::Bool(b) => b.to_string(),
                Scalar::I64(i) => i.to_string(),
                Scalar::U64(u) => u.to_string(),
                Scalar::F64(f) => f.to_string(),
            };
            (k, value)
        })
        .collect())
}

impl ConfigProvider for AppConfig {
    fn get_gear_config(&self, gear_name: &str) -> Option<&serde_json::Value> {
        self.gears.get(gear_name)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_server_name")]
    pub name: String,
    #[serde(default = "default_home_dir")]
    pub home_dir: PathBuf, // will be normalized to absolute path
}

fn default_server_name() -> String {
    "cf-gears".to_owned()
}

fn default_home_dir() -> PathBuf {
    super::host::paths::default_home_dir().join(".cf-gears")
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: default_server_name(),
            home_dir: default_home_dir(),
        }
    }
}

impl ServerConfig {
    fn normalize_home_dir_inplace(&mut self) -> Result<()> {
        self.home_dir = super::host::normalize_path(
            self.home_dir
                .to_str()
                .context("home directory configuration is not a valid path")?,
        )
        .context("home_dir normalization failed")?;

        std::fs::create_dir_all(&self.home_dir).context("Failed to create home_dir")?;

        Ok(())
    }
}

/// Console output format for the logging layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleFormat {
    /// Human-readable text output (default).
    #[default]
    Text,
    /// Structured JSON output (useful for container log collectors).
    Json,
}

/// Logging configuration - maps subsystem names to their logging settings.
/// Key "default" is the catch-all for logs that don't match explicit subsystems.
pub type LoggingConfig = HashMap<String, Section>;

/// Per-vendor configuration bag: vendor name → arbitrary JSON/YAML value.
/// Each vendor's section can be deserialized into a typed struct via
/// [`AppConfig::vendor_config`] or [`AppConfig::vendor_config_or_default`].
pub type VendorConfig = HashMap<String, serde_json::Value>;

// ================= Custom serde gear for optional Level (supports "off") =================
mod optional_level_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use tracing::Level;

    #[allow(clippy::ref_option, clippy::trivially_copy_pass_by_ref)]
    pub fn serialize<S>(level: &Option<Level>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match level {
            Some(l) => serializer.serialize_str(l.as_str()),
            None => serializer.serialize_str("off"),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Level>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "trace" => Ok(Some(Level::TRACE)),
            "debug" => Ok(Some(Level::DEBUG)),
            "info" => Ok(Some(Level::INFO)),
            "warn" => Ok(Some(Level::WARN)),
            "error" => Ok(Some(Level::ERROR)),
            "off" | "none" => Ok(None),
            _ => Err(serde::de::Error::custom(format!("invalid level: {s}"))),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn default() -> Option<Level> {
        Some(Level::INFO)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SectionFile {
    pub file: String,
    #[serde(
        default = "optional_level_serde::default",
        with = "optional_level_serde"
    )]
    pub file_level: Option<Level>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Section {
    #[serde(default)]
    pub console_format: ConsoleFormat,
    #[serde(
        default = "optional_level_serde::default",
        with = "optional_level_serde"
    )]
    pub console_level: Option<Level>,
    #[serde(flatten)]
    pub section_file: Option<SectionFile>,
    pub max_age_days: Option<u32>, // Not implemented yet
    #[serde(default)]
    pub max_backups: Option<usize>, // How many files to keep
    #[serde(default)]
    pub max_size_mb: Option<u64>, // Max size of the file in MB
}

impl Section {
    #[must_use]
    pub fn file(&self) -> Option<&str> {
        self.section_file
            .as_ref()
            .map(|f| f.file.as_str())
            .filter(|s| !s.is_empty())
    }

    #[must_use]
    pub fn file_level(&self) -> Option<Level> {
        self.section_file.as_ref().and_then(|f| f.file_level)
    }
}

/// Create a default logging configuration.
#[must_use]
pub fn default_logging_config() -> LoggingConfig {
    let mut logging = HashMap::new();
    logging.insert(
        "default".to_owned(),
        Section {
            console_level: Some(Level::INFO),
            section_file: Some(SectionFile {
                file: "logs/cf-gears.log".to_owned(),
                file_level: Some(Level::DEBUG),
            }),
            console_format: ConsoleFormat::default(),
            max_age_days: Some(7),
            max_backups: Some(3),
            max_size_mb: Some(100),
        },
    );
    logging
}

/// Remap a split, `APP__`-prefixed environment key into a figment config path.
///
/// k8s env var names must be `C_IDENTIFIERs` (no dashes), but gear names are
/// kebab-case. Under the `gears` branch we therefore remap `_` -> `-` in the
/// gear-name segment only (the one right after `gears`). Nested field names
/// (e.g. `max_age_days`) and other branches (e.g. `vendor`) are left alone; the
/// dash form is unaffected since remapping `-` is a no-op.
///
/// Safe because gear names are guaranteed kebab-case by `validate_kebab_case`
/// (`#[toolkit::gear]` macro + `make validate-gear-names`).
pub(crate) fn remap_gear_env_key(key: &str) -> String {
    // Lowercase so the match is independent of figment's own (later) lowercasing.
    let lower = key.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('.').collect();
    if parts.first() == Some(&"gears") && parts.len() >= 2 {
        let gear = parts[1].replace('_', "-");
        parts[1] = gear.as_str();
        parts.join(".")
    } else {
        lower
    }
}

/// Reject the pre-2026-03 top-level `tracing:` section with a migration hint.
///
/// The section moved under `opentelemetry:` in `8c2187bc4`, which also put
/// `deny_unknown_fields` on [`AppConfig`] — so an unmigrated config already
/// fails, but with a bare "unknown field" that says nothing about where the
/// settings went. This turns that into an actionable error.
///
/// One check covers both shapes the old documentation taught: `Env::prefixed`
/// splits on `__`, so `APP__TRACING__EXPORTER__ENDPOINT` lands on the same
/// `tracing.*` path as the YAML block.
///
/// The nested `opentelemetry.tracing` is untouched — `find_value` resolves from
/// the root — and `AppConfig::default()` contributes no `tracing` key.
///
/// # Errors
/// Returns an error when a root-level `tracing` key is present.
fn reject_legacy_tracing_key(figment: &figment::Figment) -> Result<()> {
    if figment.find_value("tracing").is_err() {
        return Ok(());
    }

    anyhow::bail!(
        "the top-level `tracing:` section was replaced by `opentelemetry:`; \
         move the settings across:\n\
         \n\
         \x20 tracing.enabled       -> opentelemetry.tracing.enabled\n\
         \x20 tracing.service_name  -> opentelemetry.resource.service_name\n\
         \x20 tracing.resource      -> opentelemetry.resource.attributes\n\
         \x20 tracing.metrics       -> opentelemetry.metrics\n\
         \x20 tracing.exporter      -> opentelemetry.exporter (shared) or \
         opentelemetry.tracing.exporter\n\
         \x20 tracing.sampler, .propagation, .http, .logs_correlation \
         -> opentelemetry.tracing.*\n\
         \n\
         Environment overrides use the same path, so APP__TRACING__* becomes \
         APP__OPENTELEMETRY__*. See docs/TRACING_SETUP.md."
    );
}

impl AppConfig {
    /// Load configuration with layered loading: defaults → YAML file → environment variables.
    /// Also normalizes `server.home_dir` into an absolute path and creates the directory.
    ///
    /// # Errors
    /// Returns an error if configuration loading or `home_dir` resolution fails.
    pub fn load_layered(config_path: &PathBuf) -> Result<Self> {
        use figment::{
            Figment,
            providers::{Env, Format, Serialized},
        };

        // For layered loading, start from AppConfig::default() which provides logging
        // defaults (via default_logging_config()); other optional sections (database,
        // opentelemetry, gears_dir) remain None unless overridden by YAML/ENV.
        let figment = Figment::new()
            .merge(Serialized::defaults(AppConfig::default()))
            .merge(StrictYaml::file(config_path))
            // Example: APP__SERVER__PORT=8087 maps to server.port.
            .merge(
                Env::prefixed("APP__")
                    .split("__")
                    .map(|key| remap_gear_env_key(key.as_str()).into()),
            );

        reject_legacy_tracing_key(&figment)?;

        let mut config: AppConfig = figment
            .extract()
            .with_context(|| "Failed to extract config from figment".to_owned())?;

        // Normalize + create home_dir immediately.
        config
            .server
            .normalize_home_dir_inplace()
            .context("Failed to resolve server.home_dir")?;

        // Merge gear files if gears_dir is specified.
        if let Some(dir) = config.gears_dir.as_ref() {
            merge_gear_files(&mut config.gears, dir)?;
        }

        Ok(config)
    }

    /// Load configuration from file or create with default values.
    /// Also normalizes `server.home_dir` into an absolute path and creates the directory.
    ///
    /// # Errors
    /// Returns an error if configuration loading or `home_dir` resolution fails.
    pub fn load_or_default(config_path: Option<&PathBuf>) -> Result<Self> {
        if let Some(path) = config_path {
            ensure!(
                path.is_file(),
                "config file does not exist: {}",
                path.to_string_lossy()
            );
            Self::load_layered(path)
        } else {
            let mut c = Self::default();
            c.server
                .normalize_home_dir_inplace()
                .context("Failed to resolve server.home_dir (defaults)")?;
            Ok(c)
        }
    }

    /// Serialize configuration to YAML.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn to_yaml(&self) -> Result<String> {
        serde_saphyr::to_string(self).context("Failed to serialize config to YAML")
    }

    /// Deserialize a vendor configuration section into a typed struct.
    ///
    /// # Errors
    /// Returns `VendorConfigError::NotFound` if the vendor is not present,
    /// or `VendorConfigError::InvalidConfig` if deserialization fails.
    pub fn vendor_config<T: DeserializeOwned>(
        &self,
        vendor_name: &str,
    ) -> Result<T, VendorConfigError> {
        let raw = self
            .vendor
            .get(vendor_name)
            .ok_or_else(|| VendorConfigError::NotFound {
                vendor: vendor_name.to_owned(),
            })?;
        T::deserialize(raw).map_err(|e| VendorConfigError::InvalidConfig {
            vendor: vendor_name.to_owned(),
            cause: e,
        })
    }

    /// Deserialize a vendor configuration section, returning `T::default()` if absent.
    ///
    /// # Errors
    /// Returns `VendorConfigError::InvalidConfig` if the section exists but cannot be
    /// deserialized into `T`.
    pub fn vendor_config_or_default<T: DeserializeOwned + Default>(
        &self,
        vendor_name: &str,
    ) -> Result<T, VendorConfigError> {
        let Some(raw) = self.vendor.get(vendor_name) else {
            return Ok(T::default());
        };
        T::deserialize(raw).map_err(|e| VendorConfigError::InvalidConfig {
            vendor: vendor_name.to_owned(),
            cause: e,
        })
    }

    /// Apply overrides from command line arguments.
    pub fn apply_cli_overrides(&mut self, verbose: u8) {
        // Set logging level based on verbose flags for "default" section.
        if let Some(default_section) = self.logging.get_mut("default") {
            default_section.console_level = match verbose {
                0 => default_section.console_level, // keep
                1 => Some(Level::DEBUG),
                _ => Some(Level::TRACE),
            };
        }
    }
}

/// Command line arguments structure.
#[derive(Debug, Clone)]
pub struct CliArgs {
    pub config: Option<String>,
    pub print_config: bool,
    pub verbose: u8,
    pub mock: bool,
}

/// Parse YAML with duplicate-key rejection.
fn strict_yaml_parse<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, serde_saphyr::Error> {
    let opts = serde_saphyr::Options {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        ..serde_saphyr::Options::default()
    };
    serde_saphyr::from_str_with_options(s, opts)
}

/// YAML [`Format`](figment::providers::Format) provider that rejects duplicate
/// mapping keys instead of silently keeping the last value.
///
/// Drop-in replacement for figment's built-in `Yaml` — use
/// `StrictYaml::file(path)` wherever you would use `Yaml::file(path)`.
struct StrictYaml;

impl figment::providers::Format for StrictYaml {
    type Error = serde_saphyr::Error;

    const NAME: &'static str = "YAML";

    fn from_str<T: serde::de::DeserializeOwned>(s: &str) -> Result<T, Self::Error> {
        strict_yaml_parse(s)
    }
}

fn merge_gear_files(
    bag: &mut HashMap<String, serde_json::Value>,
    dir: impl AsRef<Path>,
) -> Result<()> {
    use std::fs;
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ext != "yml" && ext != "yaml" {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        let raw = fs::read_to_string(&path)?;
        let json: serde_json::Value = strict_yaml_parse(&raw)
            .with_context(|| format!("failed to parse gear file: {}", path.display()))?;
        bag.insert(name, json);
    }
    Ok(())
}

// ---- New ToolKit DB Handling Functions ----

/// Expands environment variables in a DSN string.
/// Replaces `${VARNAME}` with the actual environment variable value.
///
/// # Errors
/// Returns an error if any referenced env var is missing.
pub fn expand_env_in_dsn(dsn: &str) -> Result<String> {
    toolkit_utils::var_expand::expand_env_vars(dsn).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Resolves password: if it contains ${VAR}, expands from environment variable; otherwise returns as-is.
///
/// # Errors
/// Returns an error if the referenced environment variable is not found.
pub fn resolve_password(password: Option<&str>) -> Result<Option<String>> {
    if let Some(pwd) = password {
        if pwd.starts_with("${") && pwd.ends_with('}') {
            // Extract variable name from ${VAR_NAME}
            let var_name = &pwd[2..pwd.len() - 1];
            let resolved = std::env::var(var_name).with_context(|| {
                format!("Environment variable '{var_name}' not found for password")
            })?;
            Ok(Some(resolved))
        } else {
            // Return literal password as-is
            Ok(Some(pwd.to_owned()))
        }
    } else {
        Ok(None)
    }
}

/// Validates that a DSN string is parseable by the dsn crate.
/// Note: `SQLite` DSNs have special formats that dsn crate doesn't recognize, so we skip validation for them.
///
/// # Errors
/// Returns an error if the DSN is invalid.
pub fn validate_dsn(dsn: &str) -> Result<()> {
    // Skip validation for SQLite DSNs as they use special syntax not recognized by dsn crate
    if dsn.starts_with("sqlite:") {
        return Ok(());
    }

    let _parsed = dsn::parse(dsn).map_err(|e| anyhow::anyhow!("Invalid DSN '{dsn}': {e}"))?;

    Ok(())
}

/// Resolves `SQLite` @`file()` syntax in DSN to actual file paths.
/// - `sqlite://@file(users.sqlite)` → `$HOME/.cf-gears/<gear>/users.sqlite`
/// - `sqlite://@file(/abs/path/file.db)` → use absolute path
/// - `sqlite://` or `sqlite:///` → `$HOME/.cf-gears/<gear>/<gear>.sqlite`
fn resolve_sqlite_dsn(
    dsn: &str,
    home_dir: &Path,
    gear_name: &str,
    dry_run: bool,
) -> Result<String> {
    if dsn.contains("@file(") {
        // Extract the file path from @file(...)
        if let Some(start) = dsn.find("@file(")
            && let Some(end) = dsn[start..].find(')')
        {
            let file_path = &dsn[start + 6..start + end]; // +6 for "@file("

            let resolved_path = if file_path.starts_with('/')
                || (file_path.len() > 1 && file_path.chars().nth(1) == Some(':'))
            {
                // Absolute path (Unix or Windows)
                PathBuf::from(file_path)
            } else {
                // Relative path - resolve under gear directory
                let gear_dir = home_dir.join(gear_name);
                if !dry_run {
                    std::fs::create_dir_all(&gear_dir).with_context(|| {
                        format!("Failed to create gear directory: {}", gear_dir.display())
                    })?;
                }
                gear_dir.join(file_path)
            };

            let normalized_path = normalize_path(&resolved_path);
            // For Windows absolute paths (C:/...), use sqlite:path format
            // For Unix absolute paths (/...), use sqlite://path format
            if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
                // Windows absolute path like C:/...
                return Ok(format!("sqlite:{normalized_path}"));
            }
            // Unix absolute path or relative path
            return Ok(format!("sqlite://{normalized_path}"));
        }
        return Err(anyhow::anyhow!(
            "Invalid @file() syntax in SQLite DSN: {dsn}"
        ));
    }

    // Handle empty DSN or just sqlite:// - default to gear.sqlite
    if dsn == "sqlite://" || dsn == "sqlite:///" || dsn == "sqlite:" {
        let gear_dir = home_dir.join(gear_name);
        if !dry_run {
            std::fs::create_dir_all(&gear_dir).with_context(|| {
                format!("Failed to create gear directory: {}", gear_dir.display())
            })?;
        }
        let db_path = gear_dir.join(format!("{gear_name}.sqlite"));
        let normalized_path = normalize_path(&db_path);
        // For Windows absolute paths (C:/...), use sqlite:path format
        // For Unix absolute paths (/...), use sqlite://path format
        if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
            // Windows absolute path like C:/...
            return Ok(format!("sqlite:{normalized_path}"));
        }
        // Unix absolute path or relative path
        return Ok(format!("sqlite://{normalized_path}"));
    }

    // Return DSN as-is for normal cases
    Ok(dsn.to_owned())
}

/// Builds a server-based DSN from individual fields.
/// Used when no base DSN is provided or when overriding DSN components.
/// Uses `url::Url` to properly handle percent-encoding of special characters.
fn build_server_dsn(
    scheme: &str,
    host: Option<&str>,
    port: Option<u16>,
    user: Option<&str>,
    password: Option<&str>,
    dbname: Option<&str>,
    params: &HashMap<String, String>,
) -> Result<String> {
    let host = host.unwrap_or("localhost");
    let user = user.unwrap_or("postgres"); // reasonable default for server-based DBs

    // Start with base URL
    let mut url = Url::parse(&format!("{scheme}://dummy/"))
        .with_context(|| format!("Invalid scheme: {scheme}"))?;

    // Set host (required)
    url.set_host(Some(host))
        .with_context(|| format!("Invalid host: {host}"))?;

    // Set port if provided
    if let Some(port) = port {
        url.set_port(Some(port))
            .map_err(|()| anyhow::anyhow!("Invalid port: {port}"))?;
    }

    // Set username
    url.set_username(user)
        .map_err(|()| anyhow::anyhow!("Failed to set username: {user}"))?;

    // Set password if provided
    if let Some(password) = password {
        url.set_password(Some(password))
            .map_err(|()| anyhow::anyhow!("Failed to set password"))?;
    }

    // Set database name as path (with leading slash)
    if let Some(dbname) = dbname {
        // Manually encode the dbname to handle special characters
        let encoded_dbname = urlencoding::encode(dbname);
        url.set_path(&format!("/{encoded_dbname}"));
    } else {
        url.set_path("/");
    }

    // Set query parameters
    if !params.is_empty() {
        // Use url::Url::query_pairs_mut() to properly handle encoding
        let mut query_pairs = url.query_pairs_mut();
        for (key, value) in params {
            query_pairs.append_pair(key, value);
        }
    }

    Ok(url.to_string())
}

/// Builds a `SQLite` DSN by replacing the database file path while preserving query parameters.
fn build_sqlite_dsn_with_dbname_override(
    original_dsn: &str,
    dbname: &str,
    gear_name: &str,
    home_dir: &Path,
    dry_run: bool,
) -> Result<String> {
    // Parse the original DSN to extract query parameters
    let query_params = if let Some(query_start) = original_dsn.find('?') {
        &original_dsn[query_start..]
    } else {
        ""
    };

    // Build the correct path for the database file
    let gear_dir = home_dir.join(gear_name);
    if !dry_run {
        std::fs::create_dir_all(&gear_dir)
            .with_context(|| format!("Failed to create gear directory: {}", gear_dir.display()))?;
    }
    let db_path = gear_dir.join(dbname);
    let normalized_path = normalize_path(&db_path);

    // Build the new DSN with correct format for the platform
    let dsn_base = if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
        // Windows absolute path like C:/...
        format!("sqlite:{normalized_path}")
    } else {
        // Unix absolute path or relative path
        format!("sqlite://{normalized_path}")
    };

    Ok(format!("{dsn_base}{query_params}"))
}

/// Builds a `SQLite` DSN from file/path or validates existing DSN.
/// If dbname is provided, it overrides the database file in the DSN.
///
/// # Arguments
/// * `dry_run` - If true, skip directory creation (for read-only inspection)
fn build_sqlite_dsn(
    dsn: Option<&str>,
    file: Option<&str>,
    path: Option<&PathBuf>,
    dbname: Option<&str>,
    gear_name: &str,
    home_dir: &Path,
    dry_run: bool,
) -> Result<String> {
    // If full DSN provided, resolve @file() syntax and validate
    if let Some(dsn) = dsn {
        let resolved_dsn = resolve_sqlite_dsn(dsn, home_dir, gear_name, dry_run)?;

        // If dbname is provided, we need to replace the database file path while preserving query params
        if let Some(dbname) = dbname {
            return build_sqlite_dsn_with_dbname_override(
                &resolved_dsn,
                dbname,
                gear_name,
                home_dir,
                dry_run,
            );
        }

        validate_dsn(&resolved_dsn)?;
        return Ok(resolved_dsn);
    }

    // Build from path (absolute)
    if let Some(path) = path {
        let absolute_path = if path.is_absolute() {
            path.clone()
        } else {
            home_dir.join(path)
        };
        let normalized_path = normalize_path(&absolute_path);
        // For Windows absolute paths (C:/...), use sqlite:path format
        // For Unix absolute paths (/...), use sqlite://path format
        if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
            // Windows absolute path like C:/...
            return Ok(format!("sqlite:{normalized_path}"));
        }
        // Unix absolute path or relative path
        return Ok(format!("sqlite://{normalized_path}"));
    }

    // Build from file (relative under gear dir)
    if let Some(file) = file {
        let gear_dir = home_dir.join(gear_name);
        if !dry_run {
            std::fs::create_dir_all(&gear_dir).with_context(|| {
                format!("Failed to create gear directory: {}", gear_dir.display())
            })?;
        }
        let db_path = gear_dir.join(file);
        let normalized_path = normalize_path(&db_path);
        // For Windows absolute paths (C:/...), use sqlite:path format
        // For Unix absolute paths (/...), use sqlite://path format
        if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
            // Windows absolute path like C:/...
            return Ok(format!("sqlite:{normalized_path}"));
        }
        // Unix absolute path or relative path
        return Ok(format!("sqlite://{normalized_path}"));
    }

    // Default to gear.sqlite
    let gear_dir = home_dir.join(gear_name);
    if !dry_run {
        std::fs::create_dir_all(&gear_dir)
            .with_context(|| format!("Failed to create gear directory: {}", gear_dir.display()))?;
    }
    let db_path = gear_dir.join(format!("{gear_name}.sqlite"));
    let normalized_path = normalize_path(&db_path);
    // For Windows absolute paths (C:/...), use sqlite:path format
    // For Unix absolute paths (/...), use sqlite://path format
    if normalized_path.len() > 1 && normalized_path.chars().nth(1) == Some(':') {
        // Windows absolute path like C:/...
        Ok(format!("sqlite:{normalized_path}"))
    } else {
        // Unix absolute path or relative path
        Ok(format!("sqlite://{normalized_path}"))
    }
}

/// Type alias for the complex return type of `build_final_db_for_gear`
type DbConfigResult = Result<Option<(String /* final_dsn */, PoolCfg)>>;

/// Builder for accumulating database configuration from multiple sources
#[derive(Default)]
struct DbConfigBuilder {
    dsn: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    dbname: Option<String>,
    params: HashMap<String, String>,
    pool: PoolCfg,
}

impl DbConfigBuilder {
    fn new() -> Self {
        Self::default()
    }

    /// Apply global server configuration
    fn apply_global_server(
        &mut self,
        global_server: &DbConnConfig,
        home_dir: &Path,
        gear_name: &str,
        dry_run: bool,
    ) -> Result<()> {
        // Apply global server DSN
        if let Some(global_dsn) = &global_server.dsn {
            let expanded_dsn = expand_env_in_dsn(global_dsn.expose())?;
            // For SQLite, resolve @file() syntax before validation
            let resolved_dsn = if expanded_dsn.starts_with("sqlite") {
                resolve_sqlite_dsn(&expanded_dsn, home_dir, gear_name, dry_run)?
            } else {
                expanded_dsn
            };
            validate_dsn(&resolved_dsn)?;
            self.dsn = Some(resolved_dsn);
        }

        // Apply global server fields (override DSN parts)
        if let Some(host) = &global_server.host {
            self.host = Some(host.clone());
        }
        if let Some(port) = global_server.port {
            self.port = Some(port);
        }
        if let Some(user) = &global_server.user {
            self.user = Some(user.clone());
        }
        if let Some(password) = resolve_password(
            global_server
                .password
                .as_ref()
                .map(toolkit_utils::SecretString::expose),
        )? {
            self.password = Some(password);
        }
        if let Some(dbname) = &global_server.dbname {
            self.dbname = Some(dbname.clone());
        }
        if let Some(params) = &global_server.params {
            self.params.extend(params.clone());
        }
        if let Some(pool) = &global_server.pool {
            self.pool = pool.clone();
        }

        Ok(())
    }

    /// Apply gear DSN (overrides global DSN)
    fn apply_gear_dsn(
        &mut self,
        gear_dsn: &str,
        home_dir: &Path,
        gear_name: &str,
        dry_run: bool,
    ) -> Result<()> {
        // For SQLite, resolve @file() syntax before validation
        let resolved_dsn = if gear_dsn.starts_with("sqlite") {
            resolve_sqlite_dsn(gear_dsn, home_dir, gear_name, dry_run)?
        } else {
            gear_dsn.to_owned()
        };
        validate_dsn(&resolved_dsn)?;
        self.dsn = Some(resolved_dsn);
        Ok(())
    }

    /// Apply gear fields (override everything)
    fn apply_gear_fields(&mut self, gear_db_config: &DbConnConfig) -> Result<()> {
        if let Some(host) = &gear_db_config.host {
            self.host = Some(host.clone());
        }
        if let Some(port) = gear_db_config.port {
            self.port = Some(port);
        }
        if let Some(user) = &gear_db_config.user {
            self.user = Some(user.clone());
        }
        if let Some(password) = resolve_password(
            gear_db_config
                .password
                .as_ref()
                .map(toolkit_utils::SecretString::expose),
        )? {
            self.password = Some(password);
        }
        if let Some(dbname) = &gear_db_config.dbname {
            self.dbname = Some(dbname.clone());
        }
        if let Some(params) = &gear_db_config.params {
            self.params.extend(params.clone());
        }
        if let Some(pool) = &gear_db_config.pool {
            // Gear pool settings override global ones
            if let Some(max_conns) = pool.max_conns {
                self.pool.max_conns = Some(max_conns);
            }
            if let Some(acquire_timeout) = pool.acquire_timeout {
                self.pool.acquire_timeout = Some(acquire_timeout);
            }
        }
        Ok(())
    }

    /// Check if we have any field overrides that require rebuilding the DSN
    fn has_field_overrides(&self) -> bool {
        self.host.is_some()
            || self.port.is_some()
            || self.user.is_some()
            || self.password.is_some()
            || !self.params.is_empty()
    }
}

/// Determines the database backend type (`SQLite` or server-based)
fn decide_backend(builder: &DbConfigBuilder, gear_db_config: &DbConnConfig) -> bool {
    // Always treat as SQLite if DSN starts with "sqlite", regardless of server reference
    // Also treat as SQLite if no server reference and no explicit DSN (default case)
    gear_db_config.file.is_some()
        || gear_db_config.path.is_some()
        || builder
            .dsn
            .as_ref()
            .is_some_and(|dsn| dsn.starts_with("sqlite"))
        || (gear_db_config.server.is_none() && builder.dsn.is_none())
}

/// Finalize `SQLite` DSN from builder state
fn finalize_sqlite_dsn(
    builder: &DbConfigBuilder,
    gear_db_config: &DbConnConfig,
    gear_name: &str,
    home_dir: &Path,
    dry_run: bool,
) -> Result<String> {
    build_sqlite_dsn(
        builder.dsn.as_deref(),
        gear_db_config.file.as_deref(),
        gear_db_config.path.as_ref(),
        builder.dbname.as_deref(),
        gear_name,
        home_dir,
        dry_run,
    )
}

/// Finalize server-based DSN from builder state
fn finalize_server_dsn(builder: &DbConfigBuilder, gear_name: &str) -> Result<String> {
    // Extract dbname from DSN if not provided separately
    let dbname = if let Some(dbname) = builder.dbname.as_deref() {
        dbname.to_owned()
    } else if let Some(dsn) = builder.dsn.as_ref() {
        // Try to extract dbname from DSN path
        if let Ok(parsed) = url::Url::parse(dsn) {
            let path = parsed.path();
            if path.len() > 1 {
                // Remove leading slash and return the path as dbname
                path[1..].to_string()
            } else {
                return Err(anyhow::anyhow!(
                    "Server-based database config for gear '{gear_name}' missing required 'dbname'"
                ));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Server-based database config for gear '{gear_name}' missing required 'dbname'"
            ));
        }
    } else {
        return Err(anyhow::anyhow!(
            "Server-based database config for gear '{gear_name}' missing required 'dbname'"
        ));
    };

    if builder.has_field_overrides() || builder.dsn.is_none() {
        // Build DSN from fields when we have overrides or no original DSN
        let scheme = if let Some(dsn) = &builder.dsn {
            let parsed = Url::parse(dsn)?;
            parsed.scheme().to_owned()
        } else {
            "postgresql".to_owned() // default
        };

        build_server_dsn(
            &scheme,
            builder.host.as_deref(),
            builder.port,
            builder.user.as_deref(),
            builder.password.as_deref(),
            Some(&dbname),
            &builder.params,
        )
    } else if let Some(original_dsn) = &builder.dsn {
        // Use original DSN when no field overrides (but update dbname if needed)
        if let Ok(mut parsed) = Url::parse(original_dsn) {
            // Update the path with the final dbname if it's different
            let original_dbname = parsed.path().trim_start_matches('/');
            if original_dbname != dbname {
                parsed.set_path(&format!("/{dbname}"));
            }
            Ok(parsed.to_string())
        } else {
            // Fallback to building from fields if URL parsing fails
            build_server_dsn(
                "postgresql",
                builder.host.as_deref(),
                builder.port,
                builder.user.as_deref(),
                builder.password.as_deref(),
                Some(&dbname),
                &builder.params,
            )
        }
    } else {
        // This branch should not be reachable due to the condition above
        unreachable!("final_dsn should not be None when has_field_overrides is false")
    }
}

/// Redacts password from DSN for logging
fn redact_dsn_for_logging(dsn: &str) -> Result<String> {
    if dsn.contains('@') {
        let parsed = Url::parse(dsn)?;
        let mut log_url = parsed;
        if log_url.password().is_some() {
            log_url.set_password(Some("***")).ok();
        }
        Ok(log_url.to_string())
    } else {
        Ok(dsn.to_owned())
    }
}

// ---- OoP Gear Configuration Support ----

/// Environment variable name for passing rendered gear config to `OoP` gears.
pub const TOOLKIT_MODULE_CONFIG_ENV: &str = "TOOLKIT_MODULE_CONFIG";

/// Rendered database configuration for `OoP` gears.
/// Contains both global server templates and gear-specific config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedDbConfig {
    /// Global database configuration with server templates.
    /// `OoP` gear can use these servers for reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global: Option<GlobalDatabaseConfig>,
    /// Gear-specific database configuration (already merged with server reference in master).
    /// This is the `gears.<name>.database` section after server merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gear: Option<DbConnConfig>,
}

impl RenderedDbConfig {
    /// Create a new `RenderedDbConfig` from global and gear database configurations.
    #[must_use]
    pub fn new(global: Option<GlobalDatabaseConfig>, gear: Option<DbConnConfig>) -> Self {
        Self { global, gear }
    }
}

/// Rendered gear configuration passed to `OoP` gears via environment variable.
///
/// This struct contains everything an `OoP` gear needs to initialize:
/// - Database configuration (structured, for field-by-field merge in `OoP`)
/// - Gear config section
/// - Logging configuration (for key-by-key merge in `OoP`)
/// - OpenTelemetry configuration (resource, tracing, metrics)
///
/// The runtime section is excluded as it's only relevant for the master host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenderedGearConfig {
    /// Rendered database configuration (structured, not resolved DSN).
    /// `OoP` gear will merge this with local --config using field-by-field merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<RenderedDbConfig>,
    /// Gear-specific config section (passed as-is)
    #[serde(default)]
    pub config: serde_json::Value,
    /// Logging configuration from master host.
    /// `OoP` gear will merge this with local --config (local keys override master keys).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingConfig>,
    /// OpenTelemetry configuration from master host (resource, tracing, metrics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opentelemetry: Option<OpenTelemetryConfig>,
}

impl RenderedGearConfig {
    /// Deserialize from JSON string (used when reading from env var).
    ///
    /// # Errors
    /// Returns an error if JSON parsing fails.
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("Failed to parse RenderedGearConfig from JSON")
    }

    /// Serialize to JSON string (used when passing to `OoP` gears via env var).
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("Failed to serialize RenderedGearConfig to JSON")
    }
}

/// Render gear configuration for passing to `OoP` gear via environment variable.
///
/// This function prepares a structured configuration that an `OoP` gear can use
/// to initialize itself. The configuration includes:
/// - Database configuration (structured, for field-by-field merge in `OoP`)
/// - Gear config section
/// - Logging configuration (for key-by-key merge in `OoP`)
/// - Tracing configuration for OTEL
///
/// The runtime section is excluded as it's only relevant for the master host.
///
/// `OoP` gears receive this via `TOOLKIT_MODULE_CONFIG` env var and can override
/// any section with their local --config file.
///
/// # Errors
/// Returns an error if gear configuration parsing fails.
pub fn render_gear_config_for_oop(
    app: &AppConfig,
    gear_name: &str,
    _home_dir: &std::path::Path,
) -> Result<RenderedGearConfig> {
    // Get gear's database config (with server reference, but NOT resolved to DSN).
    // OoP gear will use DbManager to resolve this with its local overrides.
    let gear_db_config = parse_gear_config(app, gear_name)
        .ok()
        .and_then(|entry| entry.database);

    // Build database config with global servers and gear config (structured, not resolved)
    let database = if gear_db_config.is_some() || app.database.is_some() {
        Some(RenderedDbConfig::new(app.database.clone(), gear_db_config))
    } else {
        None
    };

    // Get the gear's config section (excluding database and runtime)
    let config = parse_gear_config(app, gear_name)
        .map(|entry| entry.config)
        .unwrap_or_default();

    // Pass logging config from master host so OoP gears can merge with their local config
    let logging = app.logging.clone();

    // Pass OpenTelemetry config from master host so OoP gears use the same settings
    let opentelemetry = if app.opentelemetry.tracing.enabled || app.opentelemetry.metrics.enabled {
        Some(app.opentelemetry.clone())
    } else {
        None
    };

    Ok(RenderedGearConfig {
        database,
        config,
        logging: Some(logging),
        opentelemetry,
    })
}

/// Parse a gear config from the config bag.
///
/// # Errors
/// Returns an error if the gear is not found or config parsing fails.
pub fn parse_gear_config(app: &AppConfig, gear_name: &str) -> Result<GearConfig> {
    let gear_raw = app
        .gears
        .get(gear_name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Gear '{gear_name}' not found in config"))?;

    let gear_config: GearConfig = serde_json::from_value(gear_raw)?;
    Ok(gear_config)
}

/// Helper to get runtime config for a gear (if present).
///
/// # Errors
/// Returns an error if gear config parsing fails.
pub fn get_gear_runtime_config(app: &AppConfig, gear_name: &str) -> Result<Option<GearRuntime>> {
    let entry = parse_gear_config(app, gear_name)?;
    Ok(entry.runtime)
}

/// Merges global + gear DB configs into a final, validated DSN and pool config.
/// Precedence: Global DSN -> Global fields -> Gear DSN -> Gear fields (fields always win).
/// For server-based, returns error if final dbname is missing.
/// For `SQLite`, builds/normalizes sqlite DSN from file/path or uses a full DSN as-is.
///
/// # Arguments
/// * `dry_run` - If true, skip directory creation (for read-only inspection)
///
/// # Errors
/// Returns an error if database configuration is invalid or resolution fails.
pub fn build_final_db_for_gear(
    app: &AppConfig,
    gear_name: &str,
    home_dir: &Path,
    dry_run: bool,
) -> DbConfigResult {
    // Parse gear entry from raw JSON
    let Some(gear_raw) = app.gears.get(gear_name) else {
        return Ok(None); // No gear config
    };

    let gear_entry: GearConfig = serde_json::from_value(gear_raw.clone())
        .with_context(|| format!("Invalid gear config structure for '{gear_name}'"))?;

    let Some(gear_db_config) = gear_entry.database else {
        tracing::warn!(
            "Gear '{}' has no database configuration; DB capability disabled",
            gear_name
        );
        return Ok(None);
    };

    // Global database config
    let global_db_config = app.database.as_ref();

    // Build configuration using the builder pattern
    let mut builder = DbConfigBuilder::new();

    // Step 1: Apply global server config if referenced
    if let Some(server_name) = &gear_db_config.server {
        let global_server = global_db_config
            .and_then(|gc| gc.servers.get(server_name))
            .ok_or_else(|| {
                anyhow::anyhow!("Referenced server '{server_name}' not found in global config")
            })?;

        builder.apply_global_server(global_server, home_dir, gear_name, dry_run)?;
    }

    // Step 2: Apply gear DSN (override global)
    if let Some(gear_dsn) = &gear_db_config.dsn {
        builder.apply_gear_dsn(gear_dsn.expose(), home_dir, gear_name, dry_run)?;
    }

    // Step 3: Apply gear fields (override everything)
    builder.apply_gear_fields(&gear_db_config)?;

    // Determine backend type and finalize DSN
    let is_sqlite = decide_backend(&builder, &gear_db_config);

    let result_dsn = if is_sqlite {
        finalize_sqlite_dsn(&builder, &gear_db_config, gear_name, home_dir, dry_run)?
    } else {
        finalize_server_dsn(&builder, gear_name)?
    };

    // Validate final DSN
    validate_dsn(&result_dsn)?;

    // Redact password for logging
    let log_dsn = redact_dsn_for_logging(&result_dsn)?;

    tracing::info!(
        "Built final DB config for gear '{}': {}",
        gear_name,
        log_dsn
    );

    Ok(Some((result_dsn, builder.pool)))
}

/// Helper function to get gear database configuration from `AppConfig`.
/// Returns the `DbConnConfig` for a gear, or None if the gear has no database config.
#[must_use]
pub fn get_gear_db_config(app: &AppConfig, gear_name: &str) -> Option<DbConnConfig> {
    let gear_raw = app.gears.get(gear_name)?;
    let gear_entry: GearConfig = serde_json::from_value(gear_raw.clone()).ok()?;
    gear_entry.database
}

/// Helper function to resolve gear home directory.
/// Returns the path where gear-specific files (like `SQLite` databases) should be stored.
#[must_use]
pub fn gear_home(app: &AppConfig, gear_name: &str) -> PathBuf {
    PathBuf::from(&app.server.home_dir).join(gear_name)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use temp_env::with_var;
    use tempfile::tempdir;

    /// Helper: a normalized `home_dir` should be absolute and not start with '~'.
    fn is_normalized_path(p: &Path) -> bool {
        p.is_absolute() && !p.starts_with("~")
    }

    /// Helper: platform default subdirectory name.
    fn default_subdir() -> &'static str {
        ".cf-gears"
    }

    #[test]
    fn test_remap_gear_env_key() {
        // (input, expected) — input keys are the dot-joined segments figment
        // produces after `split("__")`, before its own lowercasing.
        let cases = [
            // Gear-name segment: underscores -> dashes.
            ("gears.my_gear.port", "gears.my-gear.port"),
            // Only the gear-name segment is remapped; nested fields keep '_'.
            ("gears.my_gear.max_age_days", "gears.my-gear.max_age_days"),
            // Multiple underscores in the gear name are all remapped.
            ("gears.a_b_c.field", "gears.a-b-c.field"),
            // Already-kebab gear name is unaffected (remapping '-' is a no-op).
            ("gears.my-gear.port", "gears.my-gear.port"),
            // Non-gears branches are left alone.
            ("vendor.my_vendor.key", "vendor.my_vendor.key"),
            ("server.home_dir", "server.home_dir"),
            // `gears` with no gear-name segment is left as-is.
            ("gears", "gears"),
            // Uppercase input is lowercased.
            ("GEARS.MY_GEAR.PORT", "gears.my-gear.port"),
            // Bare key without dots.
            ("server", "server"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                remap_gear_env_key(input),
                expected,
                "remap_gear_env_key({input:?})"
            );
        }
    }

    #[test]
    fn test_default_config_structure() {
        let config = AppConfig::default();

        // Database defaults (simplified structure)
        assert!(config.database.is_none());

        // Logging defaults
        let logging = config.logging;
        assert!(logging.contains_key("default"));

        let default_section = &logging["default"];
        assert_eq!(default_section.console_level, Some(Level::INFO));
        assert_eq!(default_section.file().unwrap(), "logs/cf-gears.log");

        // Gears bag is empty by default
        assert!(config.gears.is_empty());
    }

    #[test]
    fn oop_http_labels_coerce_numeric_and_bool_values_to_strings() {
        // A bare-numeric env value (e.g. `APP__OOP_HTTP__LABELS__SHARD=7`) is
        // parsed as an integer by the env layer; it must load as a string
        // label rather than aborting the config parse.
        let cfg: OopHttpConfig = serde_json::from_value(serde_json::json!({
            "listen_addr": "0.0.0.0:8080",
            "labels": {
                "shard": 7,
                "role": "ingest",
                "canary": true,
            }
        }))
        .expect("numeric/bool label values must deserialize");

        assert_eq!(cfg.labels.get("shard").map(String::as_str), Some("7"));
        assert_eq!(cfg.labels.get("role").map(String::as_str), Some("ingest"));
        assert_eq!(cfg.labels.get("canary").map(String::as_str), Some("true"));
    }

    // `#[serial]`: calls load_layered, which reads the APP__ env layer; serialize
    // against the env-override tests that mutate process-global APP__* vars.
    #[test]
    #[serial]
    fn test_load_layered_normalizes_home_dir() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");

        // Provide a user path with "~" to ensure expansion and normalization.
        let yaml = r#"
server:
  home_dir: "~/.test_cfgears"

database:
  servers:
    test_postgres:
      dsn: "postgres://user:pass@localhost/db"
      pool:
        max_conns: 20

logging:
  default:
    console_level: debug
    file: "logs/default.log"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let config = AppConfig::load_layered(&cfg_path).unwrap();

        // home_dir should be normalized immediately
        assert!(is_normalized_path(&config.server.home_dir));
        assert!(config.server.home_dir.ends_with(".test_cfgears"));

        // database parsed (TODO: update test to use new config format)
        // For now, since this test uses old format YAML, we skip DB assertions
        // let db = config.database.as_ref().unwrap();

        // logging parsed
        let logging = &config.logging;
        let def = &logging["default"];
        assert_eq!(def.console_level, Some(Level::DEBUG));
        assert_eq!(def.section_file.as_ref().unwrap().file, "logs/default.log");
    }

    #[test]
    fn test_load_or_default_normalizes_home_dir_when_none() {
        // No external file => defaults, but home_dir must be normalized.
        // Ensure platform env is present for home resolution in CI.
        let tmp = tempdir().unwrap();
        let env_var = if cfg!(target_os = "windows") {
            "APPDATA"
        } else {
            "HOME"
        };
        with_var(env_var, Some(tmp.path().to_str().unwrap()), || {
            let config = AppConfig::load_or_default(None).unwrap();
            assert!(is_normalized_path(&config.server.home_dir));
            assert!(config.server.home_dir.ends_with(default_subdir()));
        });
    }

    // `#[serial]`: load_layered reads the APP__ env layer, so `gears.is_empty()`
    // would race with the (also `#[serial]`) env-override tests that set APP__GEARS__*.
    #[test]
    #[serial]
    fn test_minimal_yaml_config() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");

        let yaml = r#"
server:
  home_dir: "~/.minimal"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let config = AppConfig::load_layered(&cfg_path).unwrap();

        // Required fields are parsed; home_dir normalized
        assert!(is_normalized_path(&config.server.home_dir));
        assert!(config.server.home_dir.ends_with(".minimal"));

        // Optional sections default to None
        assert!(config.database.is_none());
        assert!(config.gears.is_empty());
    }

    #[test]
    fn test_cli_overrides() {
        let mut config = AppConfig::default();

        let args = CliArgs {
            config: None,
            print_config: false,
            verbose: 2, // trace
            mock: false,
        };

        config.apply_cli_overrides(args.verbose);

        // Port override

        // Verbose override affects logging
        let logging = &config.logging;
        let default_section = &logging["default"];
        assert_eq!(default_section.console_level, Some(Level::TRACE));
    }

    #[test]
    fn test_cli_verbose_levels_matrix() {
        for (verbose_level, expected_log_level) in [
            (0, Some(Level::INFO)), // unchanged from default
            (1, Some(Level::DEBUG)),
            (2, Some(Level::TRACE)),
            (3, Some(Level::TRACE)), // cap at trace
        ] {
            let mut config = AppConfig::default();
            let args = CliArgs {
                config: None,
                print_config: false,
                verbose: verbose_level,
                mock: false,
            };

            config.apply_cli_overrides(args.verbose);

            let logging = &config.logging;
            let default_section = &logging["default"];

            if verbose_level == 0 {
                assert_eq!(default_section.console_level, Some(Level::INFO));
            } else {
                assert_eq!(default_section.console_level, expected_log_level);
            }
        }
    }

    // `#[serial]`: calls load_layered (reads APP__ env layer) and asserts on the
    // gears map, which the env-override tests mutate via APP__GEARS__*.
    #[test]
    #[serial]
    fn test_layered_config_loading_with_gears_dir() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("gears_dir.yaml");
        let gears_dir = tmp.path().join("gears");

        fs::create_dir_all(&gears_dir).unwrap();
        let gear_cfg = gears_dir.join("test_gear.yaml");
        fs::write(
            &gear_cfg,
            r#"
setting1: "value1"
setting2: 42
"#,
        )
        .unwrap();

        // Convert Windows paths to forward slashes for YAML compatibility
        let gears_dir_str = normalize_path(&gears_dir);
        let yaml = format!(
            r#"
server:
  home_dir: "~/.gears_test"

gears_dir: "{gears_dir_str}"

gears:
  existing_gear:
    key: "value"
"#
        );

        fs::write(&cfg_path, yaml).unwrap();

        let config = AppConfig::load_layered(&cfg_path).unwrap();

        // Should have loaded the existing gear from gears section
        assert!(config.gears.contains_key("existing_gear"));

        // Should have also loaded the gear from gears_dir
        assert!(config.gears.contains_key("test_gear"));

        // Check the loaded gear config
        let test_gear = &config.gears["test_gear"];
        assert_eq!(test_gear["setting1"], "value1");
        assert_eq!(test_gear["setting2"], 42);
    }

    // `#[serial]`: calls load_layered, which reads the APP__ env layer; serialize
    // against the env-override tests that mutate process-global APP__* vars.
    #[test]
    #[serial]
    fn test_load_and_init_logging_smoke() {
        // Just verifies structure is acceptable for logging init path.
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("logging.yaml");
        let yaml = r#"
server:
  home_dir: "~/.logging_test"

logging:
  default:
    console_level: debug
    file: ""
    file_level: info
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let config = AppConfig::load_layered(&cfg_path).unwrap();
        let logging = &config.logging;
        assert!(logging.contains_key("default"));

        let default_section = &logging["default"];
        assert_eq!(default_section.console_level, Some(Level::DEBUG));
        assert_eq!(default_section.file_level(), Some(Level::INFO));
        // not calling init to avoid side effects in tests
    }

    // ===================== DB Configuration Precedence Tests =====================

    /// Helper function to create `AppConfig` with database server configuration
    fn create_app_with_server(server_name: &str, db_config: DbConnConfig) -> AppConfig {
        let mut servers = HashMap::new();
        servers.insert(server_name.to_owned(), db_config);

        AppConfig {
            database: Some(GlobalDatabaseConfig {
                servers,
                auto_provision: None,
            }),
            ..Default::default()
        }
    }

    /// Helper function to add a gear to `AppConfig`
    fn add_gear_to_app(app: &mut AppConfig, gear_name: &str, database_config: &serde_json::Value) {
        app.gears.insert(
            gear_name.to_owned(),
            serde_json::json!({
                "database": database_config,
                "config": {}
            }),
        );
    }

    /// Helper function to add a gear with custom config to `AppConfig`
    fn add_gear_with_config(app: &mut AppConfig, gear_name: &str, config: &serde_json::Value) {
        app.gears.insert(
            gear_name.to_owned(),
            serde_json::json!({
                "database": {},
                "config": config
            }),
        );
    }

    /// Helper function to create a minimal `AppConfig` for testing
    fn create_minimal_app() -> AppConfig {
        AppConfig {
            database: None,
            gears: HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn test_precedence_global_dsn_only() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                dsn: Some(toolkit_utils::SecretString::new(
                    "postgresql://global_user:global_pass@global_host:5432/global_db",
                )),
                ..Default::default()
            },
        );

        // Gear references global server
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("global_user"));
        assert!(dsn.contains("global_host"));
        assert!(dsn.contains("global_db"));
    }

    #[test]
    fn test_precedence_global_fields_only() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("field_host".to_owned()),
                port: Some(5433),
                user: Some("field_user".to_owned()),
                dbname: Some("field_db".to_owned()),
                ..Default::default()
            },
        );

        // Gear references global server
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("field_host"));
        assert!(dsn.contains("5433"));
        assert!(dsn.contains("field_user"));
        assert!(dsn.contains("field_db"));
    }

    #[test]
    fn test_precedence_gear_dsn_only() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": "sqlite://gear_test.db?wal=true&synchronous=NORMAL"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("gear_test.db"));
        assert!(dsn.contains("wal=true"));
    }

    #[test]
    fn test_precedence_gear_fields_only() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "file": "gear_fields.db"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("gear_fields.db"));
        // Platform-specific DSN format check
        #[cfg(windows)]
        assert!(dsn.starts_with("sqlite:") && !dsn.starts_with("sqlite://"));
        #[cfg(unix)]
        assert!(dsn.starts_with("sqlite://"));
    }

    #[test]
    fn test_precedence_fields_override_dsn() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                dsn: Some(toolkit_utils::SecretString::new(
                    "postgresql://old_user:old_pass@old_host:5432/old_db",
                )),
                host: Some("new_host".to_owned()), // This should override DSN host
                port: Some(5433),                  // This should override DSN port
                user: Some("new_user".to_owned()), // This should override DSN user
                dbname: Some("new_db".to_owned()), // This should override DSN dbname
                ..Default::default()
            },
        );

        // Gear also overrides some fields
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server",
                "port": 5434  // Gear field should override global field
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        // Fields should override DSN parts
        assert!(dsn.contains("new_host"));
        assert!(dsn.contains("5434")); // Gear override should win
        assert!(dsn.contains("new_user"));
        assert!(dsn.contains("new_db"));
        // Old DSN values should not appear
        assert!(!dsn.contains("old_host"));
        assert!(!dsn.contains("5432"));
        assert!(!dsn.contains("old_user"));
        assert!(!dsn.contains("old_db"));
    }

    #[test]
    fn test_env_expansion_password() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        with_var("TEST_DB_PASSWORD", Some("secret123"), || {
            let mut app = create_app_with_server(
                "test_server",
                DbConnConfig {
                    host: Some("localhost".to_owned()),
                    port: Some(5432),
                    user: Some("testuser".to_owned()),
                    password: Some(toolkit_utils::SecretString::new("${TEST_DB_PASSWORD}")), // Should expand to "secret123"
                    dbname: Some("testdb".to_owned()),
                    ..Default::default()
                },
            );

            add_gear_to_app(
                &mut app,
                "test_gear",
                &serde_json::json!({
                    "server": "test_server"
                }),
            );

            let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
            assert!(result.is_some());

            let (dsn, _pool) = result.unwrap();
            assert!(dsn.contains("secret123"));
        });
    }

    #[test]
    fn test_env_expansion_in_dsn() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        temp_env::with_vars(
            [
                ("DB_HOST", Some("test-server")),
                ("DB_PASSWORD", Some("env_secret")),
            ],
            || {
                let mut app = create_app_with_server(
                    "test_server",
                    DbConnConfig {
                        dsn: Some(toolkit_utils::SecretString::new(
                            "postgresql://user:${DB_PASSWORD}@${DB_HOST}:5432/mydb",
                        )),
                        ..Default::default()
                    },
                );

                add_gear_to_app(
                    &mut app,
                    "test_gear",
                    &serde_json::json!({
                        "server": "test_server"
                    }),
                );

                let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
                assert!(result.is_some());

                let (dsn, _pool) = result.unwrap();
                assert!(dsn.contains("test-server"));
                assert!(dsn.contains("env_secret"));
                // ${} placeholders should be replaced
                assert!(!dsn.contains("${DB_HOST}"));
                assert!(!dsn.contains("${DB_PASSWORD}"));
            },
        );
    }

    #[test]
    fn test_sqlite_file_path_resolution() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Test 1: file (relative to home_dir/gear_name/)
        let app1 = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "file": "test.db"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result1 = build_final_db_for_gear(&app1, "test_gear", home_dir, false).unwrap();
        assert!(result1.is_some());
        let (dsn1, _) = result1.unwrap();
        assert!(dsn1.contains("test_gear"));
        assert!(dsn1.contains("test.db"));

        // Test 2: path (absolute path)
        let abs_path = tmp.path().join("absolute.db");
        let app2 = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "path": abs_path.to_string_lossy()
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result2 = build_final_db_for_gear(&app2, "test_gear", home_dir, false).unwrap();
        assert!(result2.is_some());
        let (dsn2, _) = result2.unwrap();
        assert!(dsn2.contains("absolute.db"));

        // Test 3: no file or path (should default to gear_name.sqlite)
        let app3 = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {},
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result3 = build_final_db_for_gear(&app3, "test_gear", home_dir, false).unwrap();
        assert!(result3.is_some());
        let (dsn3, _) = result3.unwrap();
        assert!(dsn3.contains("test_gear.sqlite"));
    }

    #[cfg(windows)]
    #[test]
    fn test_sqlite_path_resolution_windows() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "file": "test.db"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());
        let (dsn, _) = result.unwrap();

        // On Windows, paths should be normalized to forward slashes in DSN
        assert!(!dsn.contains('\\'));
        assert!(dsn.contains('/'));
    }

    #[test]
    fn test_sqlite_dsn_with_server_reference_and_dbname_override() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut app = AppConfig::default();

        // Global server with SQLite DSN and query params
        let mut servers = HashMap::new();
        servers.insert(
            "sqlite_users".to_owned(),
            DbConnConfig {
                engine: None,
                dsn: Some(toolkit_utils::SecretString::new(
                    "sqlite://users_info.db?WAL=true&synchronous=NORMAL&busy_timeout=5000",
                )),
                host: None,
                port: None,
                user: None,
                password: None,
                dbname: None,
                params: None,
                pool: None,
                file: None,
                path: None,
                lock_keepalive: None,
                server: None,
            },
        );

        app.database = Some(GlobalDatabaseConfig {
            servers,
            auto_provision: None,
        });

        // Gear that references the server but overrides the dbname
        app.gears.insert(
            "users_info".to_owned(),
            serde_json::json!({
                "database": {
                    "server": "sqlite_users",
                    "dbname": "users_info.db"
                },
                "config": {}
            }),
        );

        let result = build_final_db_for_gear(&app, "users_info", home_dir, false).unwrap();
        assert!(result.is_some());
        let (dsn, _) = result.unwrap();

        // Should be an absolute path with preserved query parameters
        assert!(dsn.contains("?WAL=true&synchronous=NORMAL&busy_timeout=5000"));
        assert!(dsn.contains("users_info/users_info.db"));

        // Platform-specific path format
        #[cfg(windows)]
        {
            // Windows should use sqlite:C:/path format
            assert!(dsn.starts_with("sqlite:"));
            assert!(!dsn.starts_with("sqlite://"));
        }

        #[cfg(unix)]
        {
            // Unix should use sqlite://path format
            assert!(dsn.starts_with("sqlite://"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_sqlite_path_resolution_unix() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "file": "test.db"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());
        let (dsn, _) = result.unwrap();

        // On Unix, paths should be absolute
        assert!(dsn.starts_with("sqlite://"));
        assert!(dsn.contains("/test_gear/test.db"));
    }

    #[test]
    fn test_server_based_db_missing_dbname_error() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                port: Some(5432),
                user: Some("testuser".to_owned()),
                // Missing dbname for server-based DB
                ..Default::default()
            },
        );

        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("missing required 'dbname'"));
    }

    #[test]
    fn test_gear_no_database_config() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Gear with no database section
        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "no_db_gear".to_owned(),
                    serde_json::json!({
                        "config": {
                            "some_setting": "value"
                        }
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "no_db_gear", home_dir, false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_gear_empty_database_config() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Gear with empty database section
        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "empty_db_gear".to_owned(),
                    serde_json::json!({
                        "database": null,
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "empty_db_gear", home_dir, false).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_referenced_server_not_found() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "server": "nonexistent_server"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Referenced server 'nonexistent_server' not found"));
    }

    #[test]
    fn test_dsn_validation_invalid_url() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": "invalid://not-a-valid[url"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_env_variable_not_found() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Use with_var with None to ensure the env var doesn't exist
        with_var("NONEXISTENT_PASSWORD", None::<&str>, || {
            let mut app = create_app_with_server(
                "test_server",
                DbConnConfig {
                    host: Some("localhost".to_owned()),
                    password: Some(toolkit_utils::SecretString::new("${NONEXISTENT_PASSWORD}")),
                    dbname: Some("testdb".to_owned()),
                    ..Default::default()
                },
            );

            add_gear_to_app(
                &mut app,
                "test_gear",
                &serde_json::json!({
                    "server": "test_server"
                }),
            );

            let result = build_final_db_for_gear(&app, "test_gear", home_dir, false);
            assert!(result.is_err());
            let error_msg = result.unwrap_err().to_string();
            assert!(error_msg.contains("NONEXISTENT_PASSWORD"));
        });
    }

    #[test]
    fn test_sqlite_at_file_relative_path() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": "sqlite://@file(users.db)"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("test_gear"));
        assert!(dsn.contains("users.db"));
        // Platform-specific DSN format check
        #[cfg(windows)]
        assert!(dsn.starts_with("sqlite:") && !dsn.starts_with("sqlite://"));
        #[cfg(unix)]
        assert!(dsn.starts_with("sqlite:///"));
    }

    #[test]
    fn test_sqlite_at_file_absolute_path() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();
        let abs_path = tmp.path().join("absolute_db.sqlite");

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": format!("sqlite://@file({})", abs_path.to_string_lossy())
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("absolute_db.sqlite"));
        // Platform-specific DSN format check
        #[cfg(windows)]
        assert!(dsn.starts_with("sqlite:") && !dsn.starts_with("sqlite://"));
        #[cfg(unix)]
        assert!(dsn.starts_with("sqlite:///"));
    }

    #[test]
    fn test_sqlite_empty_dsn_default() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": "sqlite://"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();
        assert!(dsn.contains("test_gear"));
        assert!(dsn.contains("test_gear.sqlite"));
        // Platform-specific DSN format check
        #[cfg(windows)]
        assert!(dsn.starts_with("sqlite:") && !dsn.starts_with("sqlite://"));
        #[cfg(unix)]
        assert!(dsn.starts_with("sqlite:///"));
    }

    #[test]
    fn test_sqlite_at_file_invalid_syntax() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let app = AppConfig {
            gears: {
                let mut gears = HashMap::new();
                gears.insert(
                    "test_gear".to_owned(),
                    serde_json::json!({
                        "database": {
                            "dsn": "sqlite://@file(missing_closing_paren"
                        },
                        "config": {}
                    }),
                );
                gears
            },
            ..Default::default()
        };

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("Invalid @file() syntax"));
    }

    #[test]
    fn test_dsn_special_characters_in_credentials() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Test with special characters in username and password
        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                port: Some(5432),
                user: Some("user@domain".to_owned()),
                password: Some(toolkit_utils::SecretString::new(
                    "pa@ss:w0rd/with%special&chars",
                )),
                dbname: Some("test/db".to_owned()),
                ..Default::default()
            },
        );

        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();

        // Verify DSN is properly encoded
        assert!(dsn.starts_with("postgresql://"));
        assert!(dsn.contains("user%40domain")); // @ encoded as %40
        assert!(dsn.contains("/test%2Fdb")); // / in dbname encoded as %2F

        // Verify DSN is parseable and contains expected user
        validate_dsn(&dsn).expect("DSN with special characters should be valid");

        // Parse the DSN to verify it contains the correct components
        let parsed_dsn = dsn::parse(&dsn).expect("DSN should be parseable");
        assert_eq!(parsed_dsn.username.as_deref(), Some("user@domain"));
        assert_eq!(
            parsed_dsn.password.as_deref(),
            Some("pa@ss:w0rd/with%special&chars")
        );
        // Note: dsn crate may have limitations with path parsing - just verify the main DSN works
        // The important thing is that the DSN is valid and contains the right components
    }

    #[test]
    #[allow(clippy::non_ascii_literal)]
    fn test_dsn_unicode_characters() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Test with Unicode characters
        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                user: Some("ユーザー".to_owned()), // Japanese characters
                dbname: Some("unicode_db".to_owned()),
                ..Default::default()
            },
        );

        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();

        // Verify DSN is properly encoded with Unicode
        assert!(dsn.starts_with("postgresql://"));
        // Unicode characters should be percent-encoded
        assert!(dsn.contains('%')); // Should contain encoded characters

        // Verify DSN is parseable
        validate_dsn(&dsn).expect("DSN with Unicode characters should be valid");
    }

    #[test]
    fn test_dsn_query_parameters_encoding() {
        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        let mut params = HashMap::new();
        params.insert("ssl mode".to_owned(), "require & verify".to_owned());
        params.insert("application_name".to_owned(), "my-app/v1.0".to_owned());

        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                user: Some("testuser".to_owned()),
                dbname: Some("testdb".to_owned()),
                params: Some(params),
                ..Default::default()
            },
        );

        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (dsn, _pool) = result.unwrap();

        // Verify query parameters are properly encoded (spaces become +, & becomes %26)
        assert!(dsn.contains("ssl+mode=require+%26+verify"));
        assert!(dsn.contains("application_name=my-app%2Fv1.0"));

        // Verify DSN is parseable
        validate_dsn(&dsn).expect("DSN with encoded query parameters should be valid");
    }

    #[test]
    fn test_pool_config_merging() {
        use std::time::Duration;

        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Global server with pool config
        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                dbname: Some("testdb".to_owned()),
                pool: Some(PoolCfg {
                    max_conns: Some(10),
                    min_conns: None,
                    acquire_timeout: Some(Duration::from_secs(5)),
                    idle_timeout: None,
                    max_lifetime: None,
                    test_before_acquire: None,
                }),
                ..Default::default()
            },
        );

        // Gear overrides only max_conns
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server",
                "pool": {
                    "max_conns": 20
                }
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (_dsn, pool) = result.unwrap();
        assert_eq!(pool.max_conns, Some(20)); // Gear override wins
        assert_eq!(pool.acquire_timeout, Some(Duration::from_secs(5))); // Global value preserved
    }

    #[test]
    fn test_pool_config_gear_overrides_all() {
        use std::time::Duration;

        let tmp = tempdir().unwrap();
        let home_dir = tmp.path();

        // Global server with pool config
        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                dbname: Some("testdb".to_owned()),
                pool: Some(PoolCfg {
                    max_conns: Some(10),
                    min_conns: None,
                    acquire_timeout: Some(Duration::from_secs(5)),
                    idle_timeout: None,
                    max_lifetime: None,
                    test_before_acquire: None,
                }),
                ..Default::default()
            },
        );

        // Gear overrides both pool settings
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server",
                "pool": {
                    "max_conns": 30,
                    "acquire_timeout": "10s"
                }
            }),
        );

        let result = build_final_db_for_gear(&app, "test_gear", home_dir, false).unwrap();
        assert!(result.is_some());

        let (_dsn, pool) = result.unwrap();
        assert_eq!(pool.max_conns, Some(30));
        assert_eq!(pool.acquire_timeout, Some(Duration::from_secs(10)));
    }

    #[test]
    fn test_list_gear_names() {
        let mut app = create_minimal_app();
        add_gear_with_config(&mut app, "zebra_gear", &serde_json::json!({}));
        add_gear_with_config(&mut app, "alpha_gear", &serde_json::json!({}));
        add_gear_with_config(&mut app, "beta_gear", &serde_json::json!({}));

        let gear_names = list_gear_names(&app);

        // Should be sorted alphabetically
        assert_eq!(gear_names.len(), 3);
        assert_eq!(gear_names[0], "alpha_gear");
        assert_eq!(gear_names[1], "beta_gear");
        assert_eq!(gear_names[2], "zebra_gear");
    }

    #[test]
    fn test_list_gear_names_empty() {
        let app = create_minimal_app();
        let gear_names = list_gear_names(&app);
        assert_eq!(gear_names.len(), 0);
    }

    #[test]
    fn test_redact_dsn_password_postgres() {
        let dsn = "postgres://user:secretpass@localhost:5432/mydb";
        let redacted = redact_dsn_password(dsn).unwrap();
        assert_eq!(
            redacted,
            "postgres://user:***REDACTED***@localhost:5432/mydb"
        );
    }

    #[test]
    fn test_redact_dsn_password_no_password() {
        let dsn = "postgres://user@localhost:5432/mydb";
        let redacted = redact_dsn_password(dsn).unwrap();
        // No password means no redaction needed
        assert_eq!(redacted, "postgres://user@localhost:5432/mydb");
    }

    #[test]
    fn test_redact_dsn_password_special_chars() {
        let dsn = "postgres://user:p@ss%40word@localhost:5432/mydb";
        let redacted = redact_dsn_password(dsn).unwrap();
        assert_eq!(
            redacted,
            "postgres://user:***REDACTED***@localhost:5432/mydb"
        );
    }

    #[test]
    fn test_render_effective_gears_config() {
        let mut app = create_minimal_app();
        add_gear_with_config(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "my_setting": "my_value",
                "enabled": true
            }),
        );

        let result = render_effective_gears_config(&app).unwrap();

        // Check structure
        assert!(result.is_object());
        let gears = result.as_object().unwrap();
        assert!(gears.contains_key("test_gear"));

        let test_gear = gears.get("test_gear").unwrap();
        assert!(test_gear.is_object());
        let test_gear_obj = test_gear.as_object().unwrap();

        // Should have config section
        assert!(test_gear_obj.contains_key("config"));

        // Check config section
        let config = test_gear_obj.get("config").unwrap();
        assert_eq!(config.get("my_setting").unwrap(), "my_value");
        assert_eq!(config.get("enabled").unwrap(), true);
    }

    #[test]
    fn test_render_effective_gears_config_with_database() {
        let mut app = create_app_with_server(
            "test_server",
            DbConnConfig {
                host: Some("localhost".to_owned()),
                port: Some(5432),
                user: Some("user".to_owned()),
                password: Some(toolkit_utils::SecretString::new("pass")),
                dbname: Some("db".to_owned()),
                ..Default::default()
            },
        );

        // Gear with database config
        add_gear_to_app(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "server": "test_server"
            }),
        );

        let result = render_effective_gears_config(&app).unwrap();
        let gears = result.as_object().unwrap();
        let test_gear = gears.get("test_gear").unwrap().as_object().unwrap();

        // Should have database section
        assert!(test_gear.contains_key("database"));
        let database = test_gear.get("database").unwrap().as_object().unwrap();
        assert!(database.contains_key("dsn"));

        // DSN should be redacted
        let dsn = database.get("dsn").unwrap().as_str().unwrap();
        assert!(dsn.contains("***REDACTED***"));
        assert!(!dsn.contains("pass"));
    }

    #[test]
    fn test_render_effective_gears_config_minimal() {
        // Test that gears with minimal/no config can be rendered
        let mut app = create_minimal_app();

        // Manually add a gear with no database or config sections
        app.gears
            .insert("minimal_gear".to_owned(), serde_json::json!({}));

        let result = render_effective_gears_config(&app).unwrap();

        // Gear should be present in output (or excluded if truly empty)
        // Either way, rendering should succeed
        assert!(result.is_object());
    }

    #[test]
    fn test_dump_effective_gears_config_yaml() {
        let mut app = create_minimal_app();
        add_gear_with_config(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "setting": "value"
            }),
        );

        let yaml = dump_effective_gears_config_yaml(&app).unwrap();

        // Should be valid YAML
        assert!(yaml.contains("test_gear:"));
        assert!(yaml.contains("config:"));
        assert!(yaml.contains("setting: value"));
    }

    #[test]
    fn test_dump_effective_gears_config_json() {
        let mut app = create_minimal_app();
        add_gear_with_config(
            &mut app,
            "test_gear",
            &serde_json::json!({
                "setting": "value"
            }),
        );

        let json = dump_effective_gears_config_json(&app).unwrap();

        // Should be valid JSON
        assert!(json.contains("\"test_gear\""));
        assert!(json.contains("\"config\""));
        assert!(json.contains("\"setting\""));
        assert!(json.contains("\"value\""));

        // Verify it's parseable
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
    }

    #[test]
    fn test_render_multiple_gears() {
        let mut app = create_minimal_app();
        add_gear_with_config(&mut app, "gear_a", &serde_json::json!({"a": 1}));
        add_gear_with_config(&mut app, "gear_b", &serde_json::json!({"b": 2}));
        add_gear_with_config(&mut app, "gear_c", &serde_json::json!({"c": 3}));

        let result = render_effective_gears_config(&app).unwrap();
        let gears = result.as_object().unwrap();

        assert_eq!(gears.len(), 3);
        assert!(gears.contains_key("gear_a"));
        assert!(gears.contains_key("gear_b"));
        assert!(gears.contains_key("gear_c"));
    }

    // ========== Vendor configuration tests ==========

    #[derive(Debug, Deserialize, Default, PartialEq)]
    struct TestVendorConfig {
        #[serde(default)]
        api_token: String,
        #[serde(default)]
        api_url: String,
    }

    #[test]
    fn test_vendor_section_parses_from_yaml() {
        let yaml = r#"
server:
  home_dir: "~/.test_vendor"
vendor:
  acme:
    api_token: "acme-token-123"
    api_url: "https://acme.example.com"
  other_corp:
    api_token: "other-token-789"
    api_url: "https://other.example.com"
"#;
        let config: AppConfig = serde_saphyr::from_str(yaml).unwrap();
        assert_eq!(config.vendor.len(), 2);
        assert!(config.vendor.contains_key("acme"));
        assert!(config.vendor.contains_key("other_corp"));

        let acme: TestVendorConfig = config.vendor_config("acme").unwrap();
        assert_eq!(acme.api_token, "acme-token-123");
        assert_eq!(acme.api_url, "https://acme.example.com");

        let other: TestVendorConfig = config.vendor_config("other_corp").unwrap();
        assert_eq!(other.api_token, "other-token-789");
        assert_eq!(other.api_url, "https://other.example.com");
    }

    #[test]
    fn test_vendor_section_defaults_to_empty() {
        let config = AppConfig::default();
        assert!(config.vendor.is_empty());
    }

    #[test]
    fn test_vendor_config_typed_access() {
        let mut config = AppConfig::default();
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({
                "api_token": "acme-token-123",
                "api_url": "https://acme.example.com"
            }),
        );

        let acme: TestVendorConfig = config.vendor_config("acme").unwrap();
        assert_eq!(acme.api_token, "acme-token-123");
        assert_eq!(acme.api_url, "https://acme.example.com");
    }

    #[test]
    fn test_vendor_config_not_found() {
        let config = AppConfig::default();
        let result: Result<TestVendorConfig, _> = config.vendor_config("nonexistent");
        assert!(matches!(
            result,
            Err(VendorConfigError::NotFound { ref vendor }) if vendor == "nonexistent"
        ));
    }

    #[test]
    fn test_vendor_config_invalid_structure() {
        let mut config = AppConfig::default();
        config
            .vendor
            .insert("bad".to_owned(), serde_json::json!("not an object"));

        let result: Result<TestVendorConfig, _> = config.vendor_config("bad");
        assert!(matches!(
            result,
            Err(VendorConfigError::InvalidConfig { ref vendor, .. }) if vendor == "bad"
        ));
    }

    #[test]
    fn test_vendor_config_or_default_missing() {
        let config = AppConfig::default();
        let acme: TestVendorConfig = config.vendor_config_or_default("acme").unwrap();
        assert_eq!(acme, TestVendorConfig::default());
    }

    #[test]
    fn test_vendor_config_or_default_present() {
        let mut config = AppConfig::default();
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({ "api_token": "acme-token-123" }),
        );

        let acme: TestVendorConfig = config.vendor_config_or_default("acme").unwrap();
        assert_eq!(acme.api_token, "acme-token-123");
    }

    #[test]
    #[serial]
    fn test_vendor_config_env_override() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_vendor"
vendor:
  env_test_vendor:
    api_token: "from_yaml"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var(
            "APP__VENDOR__ENV_TEST_VENDOR__API_TOKEN",
            Some("from_env"),
            || {
                let config = AppConfig::load_layered(&cfg_path).unwrap();
                let v: TestVendorConfig = config.vendor_config("env_test_vendor").unwrap();
                assert_eq!(v.api_token, "from_env");
            },
        );
    }

    #[test]
    #[serial]
    fn test_oop_http_labels_from_yaml_and_env() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_oop_labels"
oop_http:
  listen_addr: "0.0.0.0:8080"
  labels:
    role: "ingest"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        // Env layer adds a second label. Env keys are lower-cased by the loader,
        // so `ZONE` lands as `zone`.
        with_var("APP__OOP_HTTP__LABELS__ZONE", Some("us-east-1"), || {
            let config = AppConfig::load_layered(&cfg_path).unwrap();
            let oop = config.oop_http.expect("oop_http present");
            assert_eq!(oop.labels.get("role"), Some(&"ingest".to_owned()));
            assert_eq!(
                oop.labels.get("zone"),
                Some(&"us-east-1".to_owned()),
                "APP__OOP_HTTP__LABELS__ZONE should populate labels[zone]"
            );
        });

        // A bare-numeric env value (e.g. a StatefulSet ordinal injected as
        // `APP__OOP_HTTP__LABELS__SHARD=7`) is coerced to its string
        // representation by `de_labels_scalar_to_string` rather than failing the
        // config load, so numeric-looking labels can be set via the environment.
        with_var("APP__OOP_HTTP__LABELS__SHARD", Some("7"), || {
            let config = AppConfig::load_layered(&cfg_path).unwrap();
            let oop = config.oop_http.expect("oop_http present");
            assert_eq!(
                oop.labels.get("shard"),
                Some(&"7".to_owned()),
                "a bare-numeric env label value should be coerced to the string \"7\""
            );
        });
    }

    #[test]
    #[serial]
    fn test_gear_config_env_override_underscore_gear_name() {
        // k8s-friendly form: gear name uses underscores in the env var name,
        // which should be remapped to the kebab-case gear key.
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_gear_env_underscore"
gears:
  static-authz-plugin:
    config:
      vendor: "from_yaml"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var(
            "APP__GEARS__STATIC_AUTHZ_PLUGIN__CONFIG__VENDOR",
            Some("acme"),
            || {
                let config = AppConfig::load_layered(&cfg_path).unwrap();
                let gear = config.gears.get("static-authz-plugin").unwrap();
                assert_eq!(gear["config"]["vendor"], serde_json::json!("acme"));
            },
        );
    }

    #[test]
    #[serial]
    fn test_gear_config_env_override_dash_gear_name_backcompat() {
        // Back-compat: the existing dash form must still work.
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_gear_env_dash"
gears:
  static-authz-plugin:
    config:
      vendor: "from_yaml"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var(
            "APP__GEARS__static-authz-plugin__CONFIG__VENDOR",
            Some("acme"),
            || {
                let config = AppConfig::load_layered(&cfg_path).unwrap();
                let gear = config.gears.get("static-authz-plugin").unwrap();
                assert_eq!(gear["config"]["vendor"], serde_json::json!("acme"));
            },
        );
    }

    #[test]
    #[serial]
    fn test_gear_config_env_override_preserves_field_underscores() {
        // Underscores in nested config field names must NOT be remapped.
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_gear_env_field"
gears:
  static-authz-plugin:
    config:
      some_field: "from_yaml"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var(
            "APP__GEARS__STATIC_AUTHZ_PLUGIN__CONFIG__SOME_FIELD",
            Some("from_env"),
            || {
                let config = AppConfig::load_layered(&cfg_path).unwrap();
                let gear = config.gears.get("static-authz-plugin").unwrap();
                assert_eq!(gear["config"]["some_field"], serde_json::json!("from_env"));
            },
        );
    }

    #[test]
    #[serial]
    fn test_vendor_config_env_override_unaffected_by_gear_remap() {
        // Isolation invariant: the gear-name `_`->`-` remap must NOT touch the
        // `vendor` branch. The underscore vendor name is the canary — if the
        // remap leaked, the env override would land under a dashed
        // `env-test-vendor` key and the underscore lookup would miss it.
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_vendor_unaffected"
vendor:
  env_test_vendor:
    api_token: "from_yaml"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var(
            "APP__VENDOR__ENV_TEST_VENDOR__API_TOKEN",
            Some("from_env"),
            || {
                let config = AppConfig::load_layered(&cfg_path).unwrap();

                // The override applied to the underscore key, not a remapped one.
                let v: TestVendorConfig = config.vendor_config("env_test_vendor").unwrap();
                assert_eq!(v.api_token, "from_env");

                // Distinguishing assertion vs. test_vendor_config_env_override:
                // no dashed key was synthesized in the vendor branch.
                assert!(
                    config.vendor.contains_key("env_test_vendor"),
                    "underscore vendor key must be preserved"
                );
                assert!(
                    !config.vendor.contains_key("env-test-vendor"),
                    "gear remap leaked into the vendor branch"
                );
            },
        );
    }

    #[test]
    fn test_vendor_multiple_vendors_typed_access() {
        let mut config = AppConfig::default();
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({ "api_token": "acme-token", "api_url": "https://acme.com" }),
        );
        config.vendor.insert(
            "other_corp".to_owned(),
            serde_json::json!({ "api_token": "other-token", "api_url": "https://other.com" }),
        );

        let acme: TestVendorConfig = config.vendor_config("acme").unwrap();
        let other: TestVendorConfig = config.vendor_config("other_corp").unwrap();

        assert_eq!(acme.api_token, "acme-token");
        assert_eq!(other.api_token, "other-token");
        assert_eq!(acme.api_url, "https://acme.com");
        assert_eq!(other.api_url, "https://other.com");
    }

    #[test]
    fn test_vendor_nested_config() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct NestedVendorConfig {
            api_url: String,
            feature_flags: FeatureFlags,
        }

        #[derive(Debug, Deserialize, PartialEq)]
        struct FeatureFlags {
            beta_mode: bool,
            max_retries: u32,
        }

        let mut config = AppConfig::default();
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({
                "api_url": "https://acme.com",
                "feature_flags": {
                    "beta_mode": true,
                    "max_retries": 3
                }
            }),
        );

        let acme: NestedVendorConfig = config.vendor_config("acme").unwrap();
        assert_eq!(acme.api_url, "https://acme.com");
        assert!(acme.feature_flags.beta_mode);
        assert_eq!(acme.feature_flags.max_retries, 3);
    }

    #[test]
    fn test_vendor_config_or_default_invalid_returns_error() {
        let mut config = AppConfig::default();
        config
            .vendor
            .insert("bad".to_owned(), serde_json::json!("not an object"));

        let result: Result<TestVendorConfig, _> = config.vendor_config_or_default("bad");
        assert!(matches!(
            result,
            Err(VendorConfigError::InvalidConfig { ref vendor, .. }) if vendor == "bad"
        ));
    }

    #[test]
    fn test_vendor_config_yaml_roundtrip() {
        let mut config = AppConfig::default();
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({ "api_token": "acme-token-123" }),
        );

        let yaml = config.to_yaml().unwrap();
        assert!(yaml.contains("vendor"));
        assert!(yaml.contains("acme"));
        assert!(yaml.contains("acme-token-123"));
    }

    #[test]
    fn test_vendor_coexists_with_gears() {
        let mut config = AppConfig::default();
        config.gears.insert(
            "my_gear".to_owned(),
            serde_json::json!({ "config": { "some_setting": true } }),
        );
        config.vendor.insert(
            "acme".to_owned(),
            serde_json::json!({ "api_token": "acme-token-123" }),
        );

        assert!(config.gears.contains_key("my_gear"));
        assert!(config.vendor.contains_key("acme"));

        let acme: TestVendorConfig = config.vendor_config("acme").unwrap();
        assert_eq!(acme.api_token, "acme-token-123");
    }

    #[test]
    fn test_vendor_error_display_messages() {
        let not_found = VendorConfigError::NotFound {
            vendor: "acme".to_owned(),
        };
        assert_eq!(
            not_found.to_string(),
            "vendor 'acme' not found in configuration"
        );

        let invalid = VendorConfigError::InvalidConfig {
            vendor: "bad".to_owned(),
            cause: serde_json::from_str::<TestVendorConfig>("invalid").unwrap_err(),
        };
        let msg = invalid.to_string();
        assert!(msg.starts_with("invalid config for vendor 'bad':"));
        assert!(std::error::Error::source(&invalid).is_none());
    }

    #[test]
    fn test_vendor_empty_object_in_yaml() {
        let yaml = r#"
server:
  home_dir: "~/.test_vendor"
vendor: {}
"#;
        let config: AppConfig = serde_saphyr::from_str(yaml).unwrap();
        assert!(config.vendor.is_empty());
    }

    // ========== Legacy `tracing:` section ==========

    /// The pre-2026-03 shape must fail with a migration hint, not a bare
    /// `unknown field` from `deny_unknown_fields`.
    // `#[serial]`: sets `APP__TRACING__*` below (via the sibling env-override
    // test), which lands on the same root-level `tracing` path this test
    // asserts is absent; serialize against it and the other APP__-mutating tests.
    #[test]
    #[serial]
    fn test_legacy_tracing_section_reports_migration() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_legacy_tracing"
tracing:
  enabled: true
  service_name: "cf-gears-api"
  exporter:
    kind: "otlp_grpc"
    endpoint: "http://127.0.0.1:4317"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let result = AppConfig::load_layered(&cfg_path);
        assert!(result.is_err(), "legacy `tracing:` should be rejected");
        let msg = format!("{:?}", result.unwrap_err());

        for expected in [
            "opentelemetry",
            "opentelemetry.resource.service_name",
            "docs/TRACING_SETUP.md",
        ] {
            assert!(msg.contains(expected), "missing {expected:?} in: {msg}");
        }
    }

    /// The nested `opentelemetry.tracing` must not trip the root-level guard.
    // `#[serial]`: see rationale on `test_legacy_tracing_section_reports_migration`.
    #[test]
    #[serial]
    fn test_nested_opentelemetry_tracing_is_accepted() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_nested_tracing"
opentelemetry:
  resource:
    service_name: "cf-gears-api"
  tracing:
    enabled: true
  metrics:
    enabled: false
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let config = AppConfig::load_layered(&cfg_path).expect("nested form should load");
        assert!(config.opentelemetry.tracing.enabled);
        assert_eq!(config.opentelemetry.resource.service_name, "cf-gears-api");
    }

    /// `reject_legacy_tracing_key` must also catch the legacy shape when it
    /// arrives through the `APP__` env layer rather than the YAML file —
    /// `Env::prefixed` splits `APP__TRACING__ENABLED` onto the same
    /// `tracing.enabled` path as the old YAML block.
    // `#[serial]`: mutates the process-global `APP__TRACING__*` var.
    #[test]
    #[serial]
    fn test_legacy_tracing_env_override_reports_migration() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_legacy_tracing_env"
opentelemetry:
  resource:
    service_name: "cf-gears-api"
"#;
        fs::write(&cfg_path, yaml).unwrap();

        with_var("APP__TRACING__ENABLED", Some("true"), || {
            let result = AppConfig::load_layered(&cfg_path);
            assert!(
                result.is_err(),
                "legacy `APP__TRACING__*` override should be rejected"
            );
            let msg = format!("{:?}", result.unwrap_err());
            assert!(
                msg.contains("opentelemetry"),
                "missing migration hint in: {msg}"
            );
        });
    }

    // ========== Duplicate YAML key rejection tests ==========

    // `#[serial]`: these call `load_layered`, which reads the process-global
    // `APP__` env layer; serialize against the env-override tests that mutate
    // `APP__*` vars via `with_var`.
    #[test]
    #[serial]
    fn test_reject_duplicate_gear_names() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_dup"
gears:
  gear1:
    config: {}
  gear2:
    config: {}
  gear1:
    config: {}
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let result = AppConfig::load_layered(&cfg_path);
        assert!(result.is_err(), "duplicate gear names should be rejected");
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("duplicate") || msg.contains("Duplicate"),
            "error should mention duplicates: {msg}"
        );
    }

    #[test]
    #[serial]
    fn test_reject_duplicate_keys_in_gear_file() {
        let tmp = tempdir().unwrap();
        let gears_dir = tmp.path().join("gears.d");
        fs::create_dir_all(&gears_dir).unwrap();

        // Gear file with duplicate "config:" key
        let gear_yaml = r#"
config:
  key1: "value1"
config:
  key2: "value2"
"#;
        fs::write(gears_dir.join("bad_gear.yaml"), gear_yaml).unwrap();

        let cfg_yaml = format!(
            r#"
server:
  home_dir: "~/.test_dup_modfile"
gears_dir: "{}"
"#,
            normalize_path(&gears_dir)
        );
        let cfg_path = tmp.path().join("cfg.yaml");
        fs::write(&cfg_path, cfg_yaml).unwrap();

        let result = AppConfig::load_layered(&cfg_path);
        assert!(
            result.is_err(),
            "duplicate keys in a gear file should be rejected"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("duplicate") || msg.contains("Duplicate"),
            "error should mention duplicates: {msg}"
        );
    }

    #[test]
    #[serial]
    fn test_no_false_positive_on_unique_gears() {
        let tmp = tempdir().unwrap();
        let cfg_path = tmp.path().join("cfg.yaml");
        let yaml = r#"
server:
  home_dir: "~/.test_ok"
gears:
  gear1:
    config: {}
  gear2:
    config: {}
  gear3:
    config: {}
"#;
        fs::write(&cfg_path, yaml).unwrap();

        let result = AppConfig::load_layered(&cfg_path);
        assert!(
            result.is_ok(),
            "unique gear names should be accepted: {:?}",
            result.unwrap_err()
        );
    }
}

// Note: DB trait implementations and helper functions removed since we now use DbManager
