use super::config::{get_gear_runtime_config, render_gear_config_for_oop};
use super::host::{init_logging_unified, init_panic_tracing, normalize_path};
use super::{AppConfig, RuntimeKind};
use crate::backends::LocalProcessBackend;
use crate::runtime::{
    DbOptions, OopGearSpawnConfig, OopSpawnOptions, RunOptions, ShutdownOptions, run, shutdown,
};
use anyhow::Result;
use figment::Figment;
use figment::providers::Serialized;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Spawn a signal handler task that cancels the provided token on SIGTERM/SIGINT.
///
/// This helper consolidates signal handling logic used by both `run_server` and `run_migrate`.
/// The `context` parameter customizes log messages for better diagnostics.
fn spawn_signal_handler(cancel: CancellationToken, context: &str) {
    let context_owned = context.to_owned();
    tokio::spawn(async move {
        match shutdown::wait_for_shutdown().await {
            Ok(()) => {
                tracing::info!(target: "", "------------------");
                tracing::info!("{}: shutdown signal received", context_owned);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "{}: signal handler failed, falling back to ctrl_c()",
                    context_owned
                );
                _ = tokio::signal::ctrl_c().await;
            }
        }
        cancel.cancel();
    });
}

/// # Errors
///
/// Returns an error if:
/// - There was a critical error during initialization of the gears
/// - Problems with the database or third-party services
/// - An issue during runtime or shutdown
///
/// The TLS crypto provider is installed automatically as the first step of
/// [`init_procedure`] (idempotent), so callers do not need to invoke
/// [`super::init_crypto_provider`] explicitly.
pub async fn run_server(config: AppConfig) -> Result<()> {
    init_procedure(&config).map_err(|e| {
        tracing::error!(error = %e, "Initialization failed");
        e
    })?;
    tracing::info!("Initializing gears...");

    // Generate process-level instance ID once at startup.
    // This is shared by all gears in this process.
    let instance_id = uuid::Uuid::new_v4();
    tracing::info!(instance_id = %instance_id, "Generated process instance ID");

    // Create root cancellation token for the entire process.
    // This token drives shutdown for the gear runtime and all lifecycle/stateful gears.
    let cancel = CancellationToken::new();

    // Hook OS signals to the root token at the host level.
    // This replaces the use of ShutdownOptions::Signals inside the runtime.
    spawn_signal_handler(cancel.clone(), "server");

    // Build config provider and resolve database options
    let db_options = resolve_db_options(&config)?;

    // Create OoP backend with cancellation token - it will auto-shutdown all processes on cancel
    let oop_backend = LocalProcessBackend::new(cancel.clone());

    // Build OoP spawn configuration
    let oop_options = build_oop_spawn_options(&config, oop_backend)?;

    // Run the ToolKit runtime with the root cancellation token.
    // Shutdown is driven by the signal handler spawned above, not by ShutdownOptions::Signals.
    // OoP gears are spawned after the start phase (once grpc-hub has bound its port).
    //
    // No `internal_token_provider` is set here on purpose: this is the
    // in-process (Profile 1) host. Gear-to-gear calls resolve to LOCAL trait
    // objects through the `ClientHub` — there is no transport, so no
    // `X-ToolKit-Internal-Token` header/metadata is ever emitted and no outbound
    // platform credential is needed. The provider is only consulted inside the
    // generated REST/gRPC transport clients, which the monolith does not use for
    // co-hosted dependencies. A genuinely remote (directory-resolved) dependency
    // is the resolving-client path, which threads its own provider
    // (`cpt-cf-adr-two-plane-auth`).
    let run_options = RunOptions::new(
        Arc::new(config),
        db_options,
        ShutdownOptions::Token(cancel.clone()),
        instance_id,
    )
    .with_oop(oop_options);

    let result = run(run_options).await;

    // Graceful shutdown - flush remaining telemetry
    #[cfg(feature = "otel")]
    tracing_shutdown();

    result
}

/// Run database migrations and exit.
///
/// This mode is designed for cloud deployment workflows where database
/// migrations need to run as a separate step before starting the application.
///
/// Phases executed:
/// - Pre-init (wire runtime internals)
/// - DB migration (run all pending migrations)
///
/// The process exits after migrations complete. Any errors are reported
/// and propagated as non-zero exit codes.
///
/// # Errors
///
/// Returns an error if:
/// - No database configuration is found
/// - Gear discovery fails
/// - Pre-init phase fails
/// - Migration phase fails
///
/// The TLS crypto provider is installed automatically as the first step of
/// [`init_procedure`] (idempotent), so callers do not need to invoke
/// [`super::init_crypto_provider`] explicitly.
pub async fn run_migrate(config: AppConfig) -> Result<()> {
    init_procedure(&config).map_err(|e| {
        tracing::error!(error = %e, "Initialization failed");
        e
    })?;
    tracing::info!("Starting migration mode...");

    // Generate process-level instance ID for this migration run
    let instance_id = uuid::Uuid::new_v4();
    tracing::info!(instance_id = %instance_id, "Generated migration instance ID");

    // Create cancellation token and wire it to OS signals
    let cancel = CancellationToken::new();

    // Hook OS signals to enable graceful cancellation of migrations
    spawn_signal_handler(cancel.clone(), "migration");

    // Build database options from configuration
    let db_options = resolve_db_options(&config)?;

    // Verify we have database configuration
    if matches!(db_options, DbOptions::None) {
        anyhow::bail!("Cannot run migrations: no database configuration found");
    }

    // Discover and build the gear registry
    let registry = crate::registry::GearRegistry::discover_and_build()?;
    tracing::info!(
        gear_count = registry.gears().len(),
        "Discovered gears for migration"
    );

    // Create the host runtime
    let host = crate::runtime::HostRuntime::new(
        registry,
        Arc::new(config),
        db_options,
        Arc::new(crate::client_hub::ClientHub::new()),
        cancel,
        instance_id,
        None, // No OoP spawning during migration
    );

    // Run only the migration phases (pre-init + DB migration)
    let result = host.run_migration_phases().await;

    // Graceful shutdown - flush remaining telemetry
    #[cfg(feature = "otel")]
    tracing_shutdown();

    result?;

    tracing::info!("All migrations completed successfully");
    Ok(())
}

fn resolve_db_options(config: &AppConfig) -> Result<DbOptions> {
    if config.database.is_none() {
        tracing::warn!("No global database section found; running without databases");
        return Ok(DbOptions::None);
    }

    tracing::info!("Using DbManager with Figment-based configuration");
    let figment = Figment::new().merge(Serialized::defaults(config));
    let db_manager = Arc::new(toolkit_db::DbManager::from_figment(
        figment,
        config.server.home_dir.clone(),
    )?);
    Ok(DbOptions::Manager(db_manager))
}

/// Build `OoP` spawn configuration from `AppConfig`.
///
/// This collects all gears with `type=oop` and prepares their spawn configuration.
/// The actual spawning happens in the `HostRuntime` after the start phase.
fn build_oop_spawn_options(
    config: &AppConfig,
    backend: LocalProcessBackend,
) -> Result<Option<OopSpawnOptions>> {
    let home_dir = PathBuf::from(&config.server.home_dir);
    let mut gears = Vec::new();

    for gear_name in config.gears.keys() {
        if let Some(spawn_config) = try_build_oop_gear_config(config, gear_name, &home_dir)? {
            gears.push(spawn_config);
        }
    }

    if gears.is_empty() {
        Ok(None)
    } else {
        tracing::info!(count = gears.len(), "Prepared OoP gears for spawning");
        Ok(Some(OopSpawnOptions {
            gears,
            backend: Box::new(backend),
        }))
    }
}

/// Try to build `OoP` gear spawn config if gear is of type `OoP`
fn try_build_oop_gear_config(
    config: &AppConfig,
    gear_name: &str,
    home_dir: &Path,
) -> Result<Option<OopGearSpawnConfig>> {
    let Some(runtime_cfg) = get_gear_runtime_config(config, gear_name)? else {
        return Ok(None);
    };

    if !matches!(runtime_cfg.mod_type, RuntimeKind::Oop) {
        return Ok(None);
    }

    let exec_cfg = runtime_cfg.execution.as_ref().ok_or_else(|| {
        anyhow::anyhow!("gear '{gear_name}' is type=oop but execution config is missing")
    })?;

    let binary = normalize_path(&exec_cfg.executable_path)?;
    let spawn_args = exec_cfg.args.clone();
    let env = exec_cfg.environment.clone();

    // Render the complete gear config (with resolved DB)
    let rendered_config = render_gear_config_for_oop(config, gear_name, home_dir)?;
    let rendered_json = rendered_config.to_json()?;

    tracing::debug!(
        gear =  %gear_name,
        "Prepared OoP gear config: db={}",
        rendered_config.database.is_some()
    );

    Ok(Some(OopGearSpawnConfig {
        gear_name: gear_name.to_owned(),
        binary,
        args: spawn_args,
        env,
        working_directory: exec_cfg.working_directory.clone(),
        rendered_config_json: rendered_json,
    }))
}

/// Initialize process-wide bootstrap state from a provided `&AppConfig`.
///
/// This helper performs the common startup sequence shared by server and migration modes.
/// It does **not** load configuration; the caller is responsible for building and passing
/// a valid `AppConfig`.
///
/// Steps performed:
///
/// - initializes tracing/logging (once, guarded by a process-wide `Once`) and metrics
///   when OpenTelemetry is enabled
/// - registers the panic hook used to route panics through tracing
/// - emits a small startup span and version metadata for diagnostics
///
/// The rustls crypto provider is installed inside this function (idempotent
/// `OnceLock`), so callers do not need to invoke
/// [`super::init_crypto_provider`] separately. Direct callers of
/// [`super::init_crypto_provider`] outside the bootstrap path (e.g. ad-hoc
/// probe binaries that do not call `run_server` / `run_migrate`) are still
/// supported.
///
/// # Errors
///
/// Returns an error if OpenTelemetry tracing initialization fails while
/// tracing is enabled, or if the crypto provider installation fails.
#[cfg_attr(not(feature = "otel"), allow(clippy::unnecessary_wraps))]
pub fn init_procedure(config: &AppConfig) -> Result<()> {
    // Install the rustls crypto provider FIRST — before anything that may
    // touch TLS (OTLP exporter over HTTPS, DB connections, etc.). The call
    // is process-wide and idempotent; calling it twice is safe.
    super::init_crypto_provider()?;

    // Build OpenTelemetry layer before logging
    #[cfg(feature = "otel")]
    let otel_layer = if config.opentelemetry.tracing.enabled {
        Some(crate::telemetry::init::init_tracing(&config.opentelemetry)?)
    } else {
        None
    };
    #[cfg(not(feature = "otel"))]
    let otel_layer = None;

    // Initialize logging + otel in one Registry
    init_logging_unified(config, otel_layer);

    // Register custom panic hook to reroute panic backtrace into tracing.
    init_panic_tracing();

    // Initialize OpenTelemetry metrics (or confirm noop when disabled)
    #[cfg(feature = "otel")]
    if let Err(e) = crate::telemetry::init::init_metrics_provider(&config.opentelemetry) {
        tracing::error!(error = %e, "OpenTelemetry metrics not initialized");
    }

    // One-time connectivity probe
    #[cfg(feature = "otel")]
    if config.opentelemetry.tracing.enabled
        && let Err(e) = crate::telemetry::init::otel_connectivity_probe(&config.opentelemetry)
    {
        tracing::error!(error = %e, "OTLP connectivity probe failed");
    }

    // Smoke test span to confirm traces flow to Jaeger
    tracing::info_span!("startup_check", app = config.server.name).in_scope(|| {
        tracing::info!("startup span alive - traces should be visible in Jaeger");
    });

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        rust_version = env!("CARGO_PKG_RUST_VERSION"),
        "{} Server starting",
        config.server.name,
    );

    Ok(())
}

#[cfg(feature = "otel")]
/// Flush compatibility shutdown hooks for OpenTelemetry tracing and metrics.
///
/// This delegates to the current telemetry shutdown helpers so callers can use a
/// single bootstrap-level function during graceful shutdown.
pub fn tracing_shutdown() {
    crate::telemetry::init::shutdown_metrics();
    crate::telemetry::init::shutdown_tracing();
}
