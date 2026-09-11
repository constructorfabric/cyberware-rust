//! Out-of-process gear bootstrap library
//!
//! This gear provides reusable functionality for bootstrapping `OoP` (out-of-process)
//! `ToolKit` gears in local (non-k8s) environments.
//!
//! ## Features
//!
//! - Configuration loading using `toolkit-bootstrap`
//! - Logging initialization with tracing
//! - Lazy gRPC client to the `DirectoryService` (connects on first use, so a
//!   cold `DirectoryService` never blocks or crashes bootstrap —
//!   `cpt-cf-adr-eventual-readiness`)
//! - Gear instance registration
//! - Heartbeat management
//! - Gear lifecycle execution
//!
//! ## Shutdown Model
//!
//! Shutdown is driven by a single root `CancellationToken` per process:
//! - OS signals (SIGTERM, SIGINT, Ctrl+C) are hooked at bootstrap level
//! - The root token is passed to `RunOptions::Token` for gear runtime shutdown
//! - Background tasks (like heartbeat) use child tokens derived from the root
//!
//! On shutdown, the gear deregisters itself from the `DirectoryService` before exiting.
//!
//! ## Example
//!
//! ```rust,no_run
//! use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let opts = OopRunOptions {
//!         gear_name: "my_gear".to_string(),
//!         instance_id: None,
//!         directory_endpoint: "http://127.0.0.1:50051".to_string(),
//!         config_path: None,
//!         verbose: 0,
//!         print_config: false,
//!         heartbeat_interval_secs: 5,
//!         version: None,
//!     };
//!
//!     run_oop_with_options(opts).await
//! }
//! ```

use anyhow::{Context, Result};
use figment::{Figment, providers::Serialized};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::config::{
    AppConfig, CliArgs, LoggingConfig, RenderedDbConfig, RenderedGearConfig,
    TOOLKIT_MODULE_CONFIG_ENV,
};
use crate::bootstrap::host::{init_logging_unified, init_panic_tracing};
use crate::runtime::{
    ClientRegistration, DbOptions, OopServeOptions, RunOptions, ShutdownOptions,
    TOOLKIT_DIRECTORY_ENDPOINT_ENV, run, run_oop_serving, shutdown,
};
use cf_system_sdks::directory::{DirectoryClient, DirectoryGrpcClient};

/// Configuration options for `OoP` gear bootstrap
#[derive(Debug, Clone)]
pub struct OopRunOptions {
    /// Logical gear name (e.g., "`file-parser`")
    pub gear_name: String,

    /// Instance ID (defaults to a random UUID if None)
    pub instance_id: Option<Uuid>,

    /// Directory service gRPC endpoint (e.g., "<http://127.0.0.1:50051>")
    pub directory_endpoint: String,

    /// Path to configuration file
    pub config_path: Option<PathBuf>,

    /// Log verbosity level (0=default, 1=debug, 2=trace)
    pub verbose: u8,

    /// Print effective configuration and exit
    pub print_config: bool,

    /// Heartbeat interval in seconds (default: 5)
    pub heartbeat_interval_secs: u64,

    /// Gear version (used for `DirectoryService` registration and `OpenAPI` version).
    /// Defaults to `None`.
    pub version: Option<String>,
}

impl Default for OopRunOptions {
    fn default() -> Self {
        // Check for config path in environment variable as fallback
        let config_path = std::env::var("TOOLKIT_CONFIG_PATH").ok().map(PathBuf::from);

        // Check for directory endpoint in environment variable (set by master host)
        // This is the preferred way to get the endpoint when spawned by master host
        let directory_endpoint = std::env::var(TOOLKIT_DIRECTORY_ENDPOINT_ENV)
            .unwrap_or_else(|_| "http://127.0.0.1:50051".to_owned());

        Self {
            gear_name: String::new(),
            instance_id: None,
            directory_endpoint,
            config_path,
            verbose: 0,
            print_config: false,
            heartbeat_interval_secs: 5,
            version: None,
        }
    }
}

/// Builds the final configuration and `DbOptions` for an `OoP` gear.
///
/// Configuration merge strategy (for each section):
/// - **Database**: field-by-field merge using `DbManager` (master as base, local as override)
/// - **Logging**: key-by-key merge (each subsystem key is overridden by local)
/// - **Config**: local completely replaces master if present
///
/// The local config file (--config) can override any settings from master's `TOOLKIT_MODULE_CONFIG`.
///
/// For database, the merge happens at 3 levels:
/// 1. Global database.servers.* from master
/// 2. Gear's database section from master (gears.<name>.database)
/// 3. Gear's database section from local --config (overrides master)
#[tracing::instrument(
    level = "debug",
    skip(local_config, rendered_config),
    fields(
        has_rendered = rendered_config.is_some(),
        has_local_db = local_config.database.is_some()
    )
)]
fn build_oop_config_and_db(
    local_config: &AppConfig,
    gear_name: &str,
    rendered_config: Option<&RenderedGearConfig>,
) -> Result<(AppConfig, DbOptions)> {
    let home_dir = PathBuf::from(&local_config.server.home_dir);

    // Build final_config for gear's "config" section
    let mut final_config = if let Some(rendered) = rendered_config {
        // TOOLKIT_MODULE_CONFIG exists: use rendered config as BASE, local config as OVERRIDE
        let mut config = local_config.clone();

        // Get or create the gear entry
        let gear_entry = config
            .gears
            .entry(gear_name.to_owned())
            .or_insert_with(|| serde_json::json!({}));

        // Merge rendered.config as base, local gear config as override
        if let Some(obj) = gear_entry.as_object_mut() {
            // If local doesn't have "config" section, use rendered entirely
            // If local has "config" section, it takes precedence (local overrides master)
            if !obj.contains_key("config") || obj["config"].is_null() {
                obj.insert("config".to_owned(), rendered.config.clone());
            }
            // If local has "config", it already overrides - no action needed
        }

        debug!(
            gear =  %gear_name,
            has_rendered_db = %rendered.database.is_some(),
            has_rendered_logging = %rendered.logging.is_some(),
            "Using rendered config from master as base, local config as override"
        );

        config
    } else {
        // No TOOLKIT_MODULE_CONFIG: use local config entirely (standalone mode)
        debug!(
            gear =  %gear_name,
            "No rendered config from master, using local config entirely (standalone mode)"
        );
        local_config.clone()
    };

    // Merge logging: master logging (base) + local logging (override by key)
    final_config.logging = merge_logging_configs(
        rendered_config.as_ref().and_then(|r| r.logging.as_ref()),
        &local_config.logging,
    );

    // Telemetry comes from master when rendered (the same section init_tracing
    // reads), so the log identity cannot diverge from the OTel resource.
    if let Some(otel) = rendered_config.and_then(|r| r.opentelemetry.as_ref()) {
        final_config.opentelemetry = otel.clone();
    }

    // Build DbOptions using Figment merge + DbManager
    // This allows field-by-field merge: master db config (base) -> local db config (override)
    let db_options = build_merged_db_options(
        &home_dir,
        gear_name,
        rendered_config.as_ref().and_then(|r| r.database.as_ref()),
        local_config,
    )?;

    Ok((final_config, db_options))
}

/// Merges logging configurations: master as base, local as override (by key).
///
/// Each key in the logging `HashMap` (e.g., "default", "calculator", "sqlx")
/// is overridden by local if present.
fn merge_logging_configs(master: Option<&LoggingConfig>, local: &LoggingConfig) -> LoggingConfig {
    master
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .chain(local.clone())
        .collect()
}

/// Builds `DbOptions` by merging rendered config from master with local config.
///
/// Uses Figment to merge configurations and `DbManager` to handle the actual
/// database connection setup with field-by-field merge logic.
fn build_merged_db_options(
    home_dir: &Path,
    gear_name: &str,
    rendered_db: Option<&RenderedDbConfig>,
    local_config: &AppConfig,
) -> Result<DbOptions> {
    // Check if we have any database configuration
    let has_rendered_db = rendered_db.is_some_and(|db| db.gear.is_some() || db.global.is_some());
    let has_local_db = local_config.database.is_some()
        || local_config
            .gears
            .get(gear_name)
            .and_then(|m| m.get("database"))
            .is_some();

    if !has_rendered_db && !has_local_db {
        debug!(
            gear =  %gear_name,
            "No database config available"
        );
        return Ok(DbOptions::None);
    }

    // Build a merged configuration for DbManager:
    // 1. Start with rendered config from master (global servers + gear db)
    // 2. Overlay local config (local can override any field)

    let mut merged_config = serde_json::Map::new();

    // Step 1: Add rendered database config from master as base
    if let Some(rendered) = rendered_db {
        // Add global servers from master
        if let Some(ref global) = rendered.global {
            let global_json = serde_json::to_value(global)
                .context("Failed to serialize rendered global db config")?;
            merged_config.insert("database".to_owned(), global_json);
        }

        // Add gear's database config from master
        if let Some(ref gear_db) = rendered.gear {
            let gear_db_json = serde_json::to_value(gear_db)
                .context("Failed to serialize rendered gear db config")?;

            let mut gears = serde_json::Map::new();
            let mut gear_entry = serde_json::Map::new();
            gear_entry.insert("database".to_owned(), gear_db_json);
            gears.insert(gear_name.to_owned(), serde_json::Value::Object(gear_entry));
            merged_config.insert("gears".to_owned(), serde_json::Value::Object(gears));
        }
    }

    // Step 2: Overlay local config (local overrides master)
    // Local global database config
    if let Some(ref local_db) = local_config.database {
        let local_db_json =
            serde_json::to_value(local_db).context("Failed to serialize local global db config")?;

        // Merge with existing or replace
        if let Some(existing) = merged_config.get_mut("database") {
            merge_json_objects(existing, &local_db_json);
        } else {
            merged_config.insert("database".to_owned(), local_db_json);
        }
    }

    // Local gear database config
    if let Some(local_gear) = local_config.gears.get(gear_name)
        && let Some(local_gear_db) = local_gear.get("database")
    {
        let gears = merged_config
            .entry("gears".to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

        if let Some(gears_obj) = gears.as_object_mut() {
            let gear_entry = gears_obj
                .entry(gear_name.to_owned())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

            if let Some(gear_obj) = gear_entry.as_object_mut() {
                if let Some(existing_db) = gear_obj.get_mut("database") {
                    merge_json_objects(existing_db, local_gear_db);
                } else {
                    gear_obj.insert("database".to_owned(), local_gear_db.clone());
                }
            }
        }
    }

    debug!(
        gear =  %gear_name,
        has_rendered = %rendered_db.is_some(),
        has_local_global = %local_config.database.is_some(),
        "Building DbManager with merged config"
    );

    // Create DbManager from merged Figment
    let figment = Figment::new().merge(Serialized::defaults(serde_json::Value::Object(
        merged_config,
    )));
    let db_manager = Arc::new(
        toolkit_db::DbManager::from_figment(figment, home_dir.to_path_buf())
            .context("Failed to create DbManager from merged config")?,
    );

    Ok(DbOptions::Manager(db_manager))
}

/// Recursively merges source JSON object into target.
/// Source values override target values for matching keys.
fn merge_json_objects(target: &mut serde_json::Value, source: &serde_json::Value) {
    if let (Some(target_obj), Some(source_obj)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source_obj {
            if let Some(target_value) = target_obj.get_mut(key) {
                // Recursively merge objects, otherwise replace
                if target_value.is_object() && value.is_object() {
                    merge_json_objects(target_value, value);
                } else {
                    *target_value = value.clone();
                }
            } else {
                target_obj.insert(key.clone(), value.clone());
            }
        }
    } else {
        // If target is not an object, replace entirely
        *target = source.clone();
    }
}

/// Run an out-of-process gear with the given options
///
/// This function:
/// 1. Creates a root `CancellationToken` for the process
/// 2. Hooks OS signals (SIGTERM, SIGINT, Ctrl+C) to trigger cancellation
/// 3. Loads configuration and initializes logging
/// 4. Creates a lazy `DirectoryService` client (connects on first use, not at
///    bootstrap — `cpt-cf-adr-eventual-readiness`)
/// 5. Registers the gear instance (background presence loop, with backoff)
/// 6. Starts a background heartbeat loop (using a child token)
/// 7. Runs the gear lifecycle with `ShutdownOptions::Token`
/// 8. Deregisters from `DirectoryService` on shutdown
///
/// ## Shutdown Model
///
/// A single root cancellation token drives shutdown for the entire process.
/// OS signals are hooked at this bootstrap level (not via `ShutdownOptions::Signals`).
/// The heartbeat loop and gear runtime both observe this token tree.
///
/// # Arguments
///
/// * `opts` - Bootstrap configuration options
///
/// # Returns
///
/// * `Ok(())` - If the gear lifecycle completed successfully
/// * `Err(e)` - If any step failed
///
/// # Example
///
/// ```rust,no_run
/// use toolkit::bootstrap::oop::{OopRunOptions, run_oop_with_options};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let opts = OopRunOptions {
///         gear_name: "file-parser".to_string(),
///         instance_id: None,
///         directory_endpoint: "http://127.0.0.1:50051".to_string(),
///         config_path: None,
///         verbose: 1,
///         print_config: false,
///         heartbeat_interval_secs: 5,
///         version: None,
///     };
///
///     run_oop_with_options(opts).await
/// }
/// ```
///
/// # Errors
/// Returns an error if the `OoP` gear fails to start or run.
#[tracing::instrument(
    level = "info",
    name = "oop_bootstrap",
    skip(opts),
    fields(
        gear =  %opts.gear_name,
        directory = %opts.directory_endpoint
    )
)]
pub async fn run_oop_with_options(opts: OopRunOptions) -> Result<()> {
    // Generate instance ID if not provided
    let instance_id = opts.instance_id.unwrap_or_else(Uuid::new_v4);

    // Create root cancellation token for the entire process.
    // This token drives shutdown for the gear runtime and all background tasks.
    let cancel = CancellationToken::new();

    // Hook OS signals to the root token at bootstrap level.
    // This replaces the use of ShutdownOptions::Signals inside the runtime.
    let cancel_for_signals = cancel.clone();
    tokio::spawn(async move {
        match shutdown::wait_for_shutdown().await {
            Ok(()) => {
                info!(target: "", "------------------");
                info!("shutdown: signal received in OoP bootstrap");
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "shutdown: primary waiter failed in OoP bootstrap, falling back to ctrl_c()"
                );
                _ = tokio::signal::ctrl_c().await;
            }
        }
        cancel_for_signals.cancel();
    });

    // Prepare CLI args for AppConfig loading
    let args = CliArgs {
        config: opts
            .config_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        print_config: opts.print_config,
        verbose: opts.verbose,
        mock: false,
    };

    // Load configuration
    let mut config = AppConfig::load_or_default(opts.config_path.as_ref())?;
    config.apply_cli_overrides(args.verbose);

    // Try to read rendered gear config from master host via env var BEFORE logging init
    // so we can use the tracing config from master for OTEL
    let rendered_config = match std::env::var(TOOLKIT_MODULE_CONFIG_ENV) {
        Ok(json) => RenderedGearConfig::from_json(&json).ok(),
        Err(_) => None,
    };

    // Build final config by merging:
    // 1. Rendered config from master host (base)
    // 2. Local config file (override)
    // This also merges logging configuration for proper initialization
    let (final_config, db_options) =
        build_oop_config_and_db(&config, &opts.gear_name, rendered_config.as_ref())?;

    // Use OpenTelemetry config from rendered (master) config only.
    // OoP gears do not fall back to local config for telemetry — if the master
    // does not provide an opentelemetry section, telemetry is skipped entirely.
    #[cfg(feature = "otel")]
    let otel_cfg = rendered_config
        .as_ref()
        .and_then(|rc| rc.opentelemetry.as_ref());

    // Initialize OTEL tracing layer (if tracing is enabled)
    #[cfg(feature = "otel")]
    let otel_layer = otel_cfg
        .filter(|cfg| cfg.tracing.enabled)
        .map(crate::telemetry::init_tracing)
        .transpose()?;
    #[cfg(not(feature = "otel"))]
    let otel_layer = None;

    // Initialize OpenTelemetry metrics provider (if configured and enabled).
    // Store error to log after logging is initialized.
    #[cfg(feature = "otel")]
    let metrics_init_error = otel_cfg
        .filter(|cfg| cfg.metrics.enabled)
        .and_then(|cfg| crate::telemetry::init::init_metrics_provider(cfg).err());

    // Initialize logging with MERGED config (master base + local override)
    init_logging_unified(&final_config, otel_layer);

    // Now that logging is available, report deferred metrics init error
    #[cfg(feature = "otel")]
    if let Some(e) = metrics_init_error {
        tracing::error!(error = %e, "OpenTelemetry metrics not initialized (OoP)");
    }

    // Register custom panic hook to reroute panic backtrace into tracing.
    init_panic_tracing();

    // Now we can log - report what we received from master
    if let Some(ref rc) = rendered_config {
        info!(
            env_var = TOOLKIT_MODULE_CONFIG_ENV,
            has_database = rc.database.is_some(),
            has_config = !rc.config.is_null(),
            has_logging = rc.logging.is_some(),
            has_opentelemetry = rc.opentelemetry.is_some(),
            "Received rendered config from master host"
        );
    } else if std::env::var(TOOLKIT_MODULE_CONFIG_ENV).is_ok() {
        warn!(
            env_var = TOOLKIT_MODULE_CONFIG_ENV,
            "Failed to parse rendered config from master host, using local config only"
        );
    } else {
        debug!(
            env_var = TOOLKIT_MODULE_CONFIG_ENV,
            "No rendered config from master host, using local config only"
        );
    }

    info!(
        gear =  %opts.gear_name,
        instance_id = %instance_id,
        directory_endpoint = %opts.directory_endpoint,
        "OoP gear bootstrap starting"
    );

    // Print config and exit if requested
    if opts.print_config {
        print_config(&config);
        return Ok(());
    }

    // Create the DirectoryService client on a LAZY channel. Per
    // `cpt-cf-adr-eventual-readiness`, an OoP gear must start even when the
    // DirectoryService is not yet reachable; blocking or crashing on it at
    // bootstrap would make it a hard startup dependency (the SPOF the ADR
    // rejects). A lazy channel performs no eager connection: it fails fast only
    // on a malformed endpoint and defers the connect to the first RPC.
    //
    // Recovery from a cold directory differs by lifecycle:
    // - `oop_http` configured: the presence loop retries registration with
    //   exponential backoff (100ms -> 30s, forever).
    // - Legacy path (no `oop_http`): only a fixed-interval heartbeat runs, with
    //   no registration retry — it relies on an external orchestrator.
    //
    // When a platform-plane credential is configured, it is attached (as
    // `x-toolkit-internal-token`) to outbound system calls via an
    // InternalAuthInterceptor (`cpt-cf-adr-two-plane-auth`).
    info!(
        "Creating directory service client (lazy connect) for {}",
        opts.directory_endpoint
    );
    let internal_auth_cfg = final_config
        .oop_http
        .as_ref()
        .and_then(|h| h.internal_auth.as_ref());
    let (directory_client, internal_token_provider) =
        build_directory_client(&opts.directory_endpoint, internal_auth_cfg, &cancel).await?;
    let directory_api: Arc<dyn DirectoryClient> = Arc::new(directory_client);

    info!("Directory service client ready (will connect on first use)");

    // Capture OoP HTTP config (if any) before moving the config into the provider.
    let oop_http = final_config.oop_http.clone();

    // Build config provider for gears
    let config_provider = Arc::new(final_config);

    // The DirectoryClient (gRPC client) is injected into the ClientHub so gears can access it.
    // `oop` is left unset: OoP gears don't spawn other OoP gears.
    let run_options = RunOptions::new(
        config_provider,
        db_options,
        ShutdownOptions::Token(cancel.clone()),
        instance_id,
    )
    .with_clients(vec![ClientRegistration::new::<dyn DirectoryClient>(
        Arc::clone(&directory_api),
    )])
    .with_internal_token_provider(internal_token_provider);

    // When `oop_http` is configured, run the HTTP-serving lifecycle:
    // Axum server + probes + directory presence (registration + heartbeat +
    // self-heal, owned by `presence_loop`) + dependency resolution + drain.
    // Otherwise fall back to the legacy gRPC-only lifecycle.
    let result = if let Some(http_cfg) = oop_http {
        info!("Starting OoP HTTP-serving lifecycle");
        let serve = build_oop_serve_options(
            &http_cfg,
            &opts.gear_name,
            instance_id,
            opts.version.clone(),
            Duration::from_secs(opts.heartbeat_interval_secs),
            Arc::clone(&directory_api),
        )
        .await?;
        run_oop_serving(run_options, serve).await
    } else {
        info!("Starting gear lifecycle (legacy gRPC-only)");
        // Legacy path: presence is not self-managed by an HTTP serve lifecycle,
        // so run a standalone heartbeat on a child token. (A no-op unless the
        // instance is registered by an external orchestrator.)
        let heartbeat_directory = Arc::clone(&directory_api);
        let heartbeat_gear = opts.gear_name.clone();
        let heartbeat_instance_id_str = instance_id.to_string();
        let heartbeat_interval = Duration::from_secs(opts.heartbeat_interval_secs.max(1));
        let heartbeat_cancel = cancel.child_token();
        tokio::spawn(async move {
            info!(interval_secs = ?heartbeat_interval, "Starting legacy heartbeat loop");
            loop {
                tokio::select! {
                    () = heartbeat_cancel.cancelled() => {
                        info!("Heartbeat loop stopping due to cancellation");
                        break;
                    }
                    () = sleep(heartbeat_interval) => {
                        if let Err(e) = heartbeat_directory
                            .send_heartbeat(&heartbeat_gear, &heartbeat_instance_id_str)
                            .await
                        {
                            warn!(error = %e, "Failed to send heartbeat, will retry");
                        }
                    }
                }
            }
        });
        run(run_options).await
    };

    if let Err(ref e) = result {
        error!(error = %e, "Gear runtime failed");
    } else {
        info!("Gear runtime completed successfully");
    }

    result
}

/// Build [`OopServeOptions`] from configuration.
///
/// The tenant-plane `BearerAuthenticator` injection point is left unset here;
/// the app/gear binary supplies that adapter when it needs the tenant-plane
/// middleware installed. The platform-plane authenticator is constructed from
/// `oop_http.internal_auth` when the `k8s-auth` feature is enabled.
async fn build_oop_serve_options(
    cfg: &super::config::OopHttpConfig,
    gear_name: &str,
    instance_id: Uuid,
    version: Option<String>,
    heartbeat_interval: Duration,
    directory: Arc<dyn DirectoryClient>,
) -> Result<OopServeOptions> {
    let listen_addr: std::net::SocketAddr = cfg
        .listen_addr
        .parse()
        .with_context(|| format!("invalid oop_http.listen_addr: {}", cfg.listen_addr))?;

    let probe_bind_addr = cfg
        .probe_bind_addr
        .as_deref()
        .map(|s| {
            s.parse::<std::net::SocketAddr>()
                .with_context(|| format!("invalid oop_http.probe_bind_addr: {s}"))
        })
        .transpose()?;

    let advertise_uri = cfg
        .advertise_uri
        .clone()
        .unwrap_or_else(|| default_advertise_uri(listen_addr));

    // Fail fast on a malformed or unreachable advertise_uri rather than only
    // when the directory rejects the registration — the same late-failure the
    // label validation below avoids.
    validate_advertise_uri(&advertise_uri, cfg.allow_loopback_advertise)?;

    // Fail fast on a mis-configured label (bad charset, over-long, too many)
    // at start-up, using the same shared rules the directory enforces. Without
    // this the process would come up, attempt to register, and only then be
    // permanently rejected by the directory — a confusing late failure for what
    // is a static configuration error. Checked before authenticator init so a
    // static config error is reported without any async backend setup.
    cf_system_sdks::directory::validate_labels(&cfg.labels).with_context(|| {
        "invalid oop_http.labels: label keys/values must be <=63 chars, <=64 entries, and use \
         only ASCII alphanumerics plus '-', '_', '.' (starting and ending alphanumeric)"
    })?;

    let internal_authenticator = build_internal_authenticator(cfg.internal_auth.as_ref()).await?;

    Ok(OopServeOptions {
        gear_name: gear_name.to_owned(),
        instance_id: instance_id.to_string(),
        version,
        advertise_uri,
        listen_addr,
        probe_bind_addr,
        drain_timeout: Duration::from_secs(cfg.drain_timeout_secs),
        heartbeat_interval,
        healthcheck_timeout: Duration::from_millis(cfg.healthcheck_timeout_ms),
        directory,
        bearer_authenticator: None,
        internal_authenticator,
        labels: cfg.labels.clone(),
    })
}

/// Construct the inbound platform-plane authenticator from configuration.
///
/// The `shared_secret` provider is built here directly (no external backend).
/// The `kube` provider is built (with the default positive/negative
/// validation cache) by the single shared
/// `toolkit_k8s_auth::build_cached_k8s_authenticator` helper also used by
/// `grpc-hub`, so the two never drift on what `provider: kube` means or how
/// it is cached. The `kube` provider requires the `k8s-auth` feature; without
/// it, `provider: kube` is an error rather than a silent enforcement
/// downgrade, as is any other unrecognized provider value. `Ok(None)` is
/// returned only when no `internal_auth` is configured at all (Profile 1).
#[cfg_attr(not(feature = "k8s-auth"), allow(clippy::unused_async))]
async fn build_internal_authenticator(
    cfg: Option<&toolkit_security::InternalAuthConfig>,
) -> Result<Option<toolkit_security::DynInternalAuthenticator>> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };

    // Dependency-light providers (shared-secret) build directly here.
    if let Some(authenticator) = cfg.build_authenticator() {
        info!("Initializing shared-secret platform-plane authenticator");
        return Ok(Some(authenticator));
    }

    #[cfg(feature = "k8s-auth")]
    {
        if cfg.is_kube() {
            info!("Initializing Kubernetes TokenReview platform-plane authenticator");
            let audiences = cfg.kube_audiences().unwrap_or_default().to_vec();
            let authenticator = toolkit_k8s_auth::build_cached_k8s_authenticator(
                audiences,
                Some(toolkit_security::DEFAULT_TOKEN_REVIEW_CACHE_TTL),
            )
            .await
            .context("failed to initialize Kubernetes TokenReview authenticator")?;
            return Ok(Some(authenticator));
        }
    }
    #[cfg(not(feature = "k8s-auth"))]
    {
        if cfg.is_kube() {
            anyhow::bail!("oop_http.internal_auth provider=kube requires the `k8s-auth` feature");
        }
    }

    anyhow::bail!(
        "internal_auth is configured but no authenticator could be built for the selected provider"
    )
}

/// Build the `OoP` gear's `DirectoryService` client on a **lazy** channel, plus
/// the outbound
/// [`InternalTokenProvider`](toolkit_contract::runtime::config::InternalTokenProvider)
/// when a platform-plane credential is configured (`None` => no credential).
///
/// The lazy channel performs no eager connection, so this succeeds even when
/// `directory_endpoint` is unreachable (`cpt-cf-adr-eventual-readiness`).
/// Interceptor and provider come from one credential source (see
/// [`build_platform_credentials`]) so they can't diverge after a rotation.
///
/// # Errors
/// Only if the endpoint is malformed or a configured credential source fails to
/// initialise — never for an unreachable directory. The endpoint is validated
/// before any credential work, so a malformed endpoint is reported ahead of a
/// credential-source failure.
async fn build_directory_client(
    directory_endpoint: &str,
    internal_auth_cfg: Option<&toolkit_security::InternalAuthConfig>,
    cancel: &CancellationToken,
) -> Result<(
    DirectoryGrpcClient,
    Option<toolkit_contract::runtime::config::InternalTokenProvider>,
)> {
    // Validate the endpoint up front — before any credential-source work — so a
    // malformed endpoint is reported ahead of a credential failure. The lazy
    // client performs no connection, only URI validation; it is reused as-is on
    // the no-credential path.
    let client = DirectoryGrpcClient::connect_lazy(directory_endpoint)?;

    let Some(cfg) = internal_auth_cfg else {
        return Ok((client, None));
    };

    let (interceptor, provider) = build_platform_credentials(cfg, cancel).await?;
    // Rebuild with the interceptor attached (it must be set at construction);
    // the endpoint was already validated above.
    let client =
        DirectoryGrpcClient::connect_lazy_with_interceptor(directory_endpoint, interceptor)?;
    Ok((client, provider))
}

/// Build the platform-plane credential material from config: the **outbound**
/// gRPC [`InternalAuthInterceptor`] (for `DirectoryService` calls) and the
/// transport-agnostic
/// [`InternalTokenProvider`](toolkit_contract::runtime::config::InternalTokenProvider)
/// that `#[toolkit::provides]` clients attach as `X-ToolKit-Internal-Token` on
/// platform-plane methods (`cpt-cf-adr-two-plane-auth`).
///
/// Both derive from ONE credential source so they never disagree after a
/// rotation:
/// - `shared_secret` → a static token for both.
/// - `kube` with `token_path` → one [`ServiceAccountTokenReader`] drives both;
///   its "no token yet" maps to [`CredentialState::Unavailable`] so an attach
///   site warns rather than silently sending nothing.
/// - `kube` without `token_path` → inbound-only: disabled interceptor + `None`
///   provider (warned).
///
/// The `match` is exhaustive so a future variant fails to compile here rather
/// than silently downgrading to no outbound credential.
async fn build_platform_credentials(
    cfg: &toolkit_security::InternalAuthConfig,
    cancel: &CancellationToken,
) -> Result<(
    toolkit_transport_grpc::InternalAuthInterceptor,
    Option<toolkit_contract::runtime::config::InternalTokenProvider>,
)> {
    use secrecy::SecretString;
    use toolkit_contract::runtime::config::{CredentialState, InternalTokenProvider};
    use toolkit_security::InternalAuthConfig;
    use toolkit_transport_grpc::{
        DEFAULT_REFRESH_INTERVAL, InternalAuthInterceptor, ServiceAccountTokenReader,
    };

    match cfg {
        InternalAuthConfig::SharedSecret { secret, .. } => {
            let token = SecretString::from(secret.clone());
            Ok((
                InternalAuthInterceptor::from_token(token.clone()),
                Some(InternalTokenProvider::from_token(token)),
            ))
        }
        InternalAuthConfig::Kube {
            token_path: Some(path),
            ..
        } => {
            let reader = ServiceAccountTokenReader::with_cancellation(
                path,
                DEFAULT_REFRESH_INTERVAL,
                cancel.child_token(),
            )
            .await
            .context("failed to read projected service-account token for outbound credential")?;
            let interceptor = reader.interceptor();
            // The reader yields `None` while the projected file is empty or the
            // refresh has not populated it; surface that as `Unavailable` (warn)
            // rather than `NotConfigured` (silent).
            let token_fn = reader.token_provider();
            let provider = InternalTokenProvider::new(move || match token_fn() {
                Some(token) => CredentialState::Available(token),
                None => CredentialState::Unavailable(
                    "projected service-account token is currently unavailable \
                     (file empty or not yet read)"
                        .into(),
                ),
            });
            Ok((interceptor, Some(provider)))
        }
        InternalAuthConfig::Kube {
            token_path: None, ..
        } => {
            warn!(
                "oop_http.internal_auth: provider=kube without token_path - this participant \
                 validates inbound platform tokens but will attach NO outbound credential"
            );
            Ok((InternalAuthInterceptor::disabled(), None))
        }
    }
}

/// Derive a default advertise URI from the bind address. Unspecified hosts
/// (`0.0.0.0` / `[::]`) are rewritten to loopback, and every IPv6 host is
/// enclosed in brackets so the resulting URI is valid when registered as a
/// REST endpoint.
fn default_advertise_uri(listen_addr: std::net::SocketAddr) -> String {
    let host = match listen_addr {
        std::net::SocketAddr::V4(addr) if addr.ip().is_unspecified() => "127.0.0.1".to_owned(),
        std::net::SocketAddr::V4(addr) => addr.ip().to_string(),
        std::net::SocketAddr::V6(addr) if addr.ip().is_unspecified() => "[::1]".to_owned(),
        std::net::SocketAddr::V6(addr) => format!("[{}]", addr.ip()),
    };
    format!("http://{host}:{}", listen_addr.port())
}

/// Reject a malformed or unreachable `advertise_uri` at start-up (fail fast).
///
/// Checks shape (parseable `http`/`https` URL, non-empty host, no userinfo) and,
/// unless `allow_loopback`, rejects a loopback / unspecified host - a
/// registered-but-unreachable instance in multi-host Profile 2 / Profile 3
/// (`cpt-cf-adr-instance-addressable-discovery` section 5). The default derives
/// loopback from an unspecified bind, so this also covers an *unset* value. The
/// directory enforces its full endpoint ruleset server-side.
fn validate_advertise_uri(uri: &str, allow_loopback: bool) -> Result<()> {
    let parsed = url::Url::parse(uri)
        .with_context(|| format!("invalid oop_http.advertise_uri: not a valid URL: {uri}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!(
            "invalid oop_http.advertise_uri: scheme must be http or https (got '{}')",
            parsed.scheme()
        );
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        anyhow::bail!("invalid oop_http.advertise_uri: missing host: {uri}");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!("invalid oop_http.advertise_uri: must not contain userinfo: {uri}");
    }
    let is_loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !allow_loopback && is_loopback {
        anyhow::bail!(
            "invalid oop_http.advertise_uri: '{uri}' is a loopback/unspecified address, which is \
             unreachable by other gears in multi-host Profile 2 / Profile 3 (a registered-but-\
             unreachable instance). Set oop_http.advertise_uri to a routable host, or set \
             oop_http.allow_loopback_advertise = true for single-host / local-dev."
        );
    }
    Ok(())
}

#[allow(unknown_lints, de1301_no_print_macros)] // direct stdout config print before exit
fn print_config(config: &AppConfig) {
    match config.to_yaml() {
        Ok(yaml) => {
            println!("{yaml}");
        }
        Err(e) => {
            eprintln!("Failed to render config as YAML: {e}");
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "oop_tests.rs"]
mod tests;
