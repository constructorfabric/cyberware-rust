use super::super::config::{ConsoleFormat, LoggingConfig, Section};
use anyhow::Context;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use tracing_subscriber::{Layer, fmt};

#[cfg(feature = "otel")]
use super::log_correlation;

// ========== OTEL-agnostic layer type (compiles with/without the feature) ==========
#[cfg(feature = "otel")]
pub type OtelLayer = tracing_opentelemetry::OpenTelemetryLayer<
    tracing_subscriber::Registry,
    opentelemetry_sdk::trace::Tracer,
>;
#[cfg(not(feature = "otel"))]
pub type OtelLayer = ();

// The JSON event formatter used by the console and file sinks.
//
// `fmt::layer().json()` configures the event format through the layer builder,
// but replacing that format (to add trace ids) discards those settings — so the
// `Format` is built by `base_json_format()` and installed via `.event_format()`.
//
// Generic over the timer so the caller never has to name the (private-ish)
// `Rfc3339` type behind `UtcTime::rfc_3339()`.
#[cfg(feature = "otel")]
type JsonEventFormat<T> = log_correlation::TraceIdJson<T>;
#[cfg(not(feature = "otel"))]
type JsonEventFormat<T> = fmt::format::Format<fmt::format::Json, T>;

/// Wrap the JSON format so records carry `trace_id` / `span_id` when enabled.
#[cfg_attr(not(feature = "otel"), allow(unused_variables))]
fn json_event_format<T>(
    base: fmt::format::Format<fmt::format::Json, T>,
    inject_trace_ids: bool,
) -> JsonEventFormat<T> {
    #[cfg(feature = "otel")]
    {
        log_correlation::TraceIdJson::new(base, inject_trace_ids)
    }
    #[cfg(not(feature = "otel"))]
    {
        base
    }
}

// ================= level helpers =================

/// Returns true if target == `crate_name` or target starts with "`crate_name::`"
fn matches_crate_prefix(target: &str, crate_name: &str) -> bool {
    target == crate_name
        || (target.starts_with(crate_name) && target[crate_name.len()..].starts_with("::"))
}

// ================= rotating writer for files =================

use file_rotate::{
    ContentLimit, FileRotate,
    compression::Compression,
    suffix::{AppendTimestamp, FileLimit},
};

#[derive(Clone)]
struct RotWriter(Arc<Mutex<FileRotate<AppendTimestamp>>>);

impl<'a> fmt::MakeWriter<'a> for RotWriter {
    type Writer = RotWriterHandle;
    fn make_writer(&'a self) -> Self::Writer {
        RotWriterHandle(self.0.clone())
    }
}

#[derive(Clone)]
struct RotWriterHandle(Arc<Mutex<FileRotate<AppendTimestamp>>>);

impl Write for RotWriterHandle {
    // NOTE: Each call acquires/releases the lock independently. Callers needing
    // atomicity for multi-part writes should use write_all() or write_fmt().
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().flush()
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.lock().write_all(buf)
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        self.0.lock().write_fmt(args)
    }
}

// A writer handle that may be None (drops writes)
#[derive(Clone)]
struct RoutedWriterHandle(Option<RotWriterHandle>);

impl Write for RoutedWriterHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(w) = &mut self.0 {
            w.write(buf)
        } else {
            Ok(buf.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(w) = &mut self.0 {
            w.flush()
        } else {
            Ok(())
        }
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if let Some(w) = &mut self.0 {
            w.write_all(buf)
        } else {
            Ok(())
        }
    }
    fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        if let Some(w) = &mut self.0 {
            w.write_fmt(args)
        } else {
            Ok(())
        }
    }
}

/// Route log records to different files by target prefix:
/// keys are *full* prefixes like "`cf-gears::api_gateway`"
#[derive(Clone)]
struct MultiFileRouter {
    default: Option<RotWriter>, // default file (from "default" section), optional
    by_prefix: Vec<(String, RotWriter)>, // subsystem → writer, sorted longest-prefix-first
}

impl MultiFileRouter {
    fn resolve_for(&self, target: &str) -> Option<RotWriterHandle> {
        for (crate_name, wr) in &self.by_prefix {
            if matches_crate_prefix(target, crate_name) {
                return Some(RotWriterHandle(wr.0.clone()));
            }
        }
        self.default.as_ref().map(|w| RotWriterHandle(w.0.clone()))
    }

    fn is_empty(&self) -> bool {
        self.default.is_none() && self.by_prefix.is_empty()
    }
}

impl<'a> fmt::MakeWriter<'a> for MultiFileRouter {
    type Writer = RoutedWriterHandle;

    fn make_writer(&'a self) -> Self::Writer {
        RoutedWriterHandle(self.default.as_ref().map(|w| RotWriterHandle(w.0.clone())))
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        let target = meta.target();
        RoutedWriterHandle(self.resolve_for(target))
    }
}

// ================= config extraction =================

struct ConfigData<'a> {
    default_section: Option<&'a Section>,
    crate_sections: Vec<(String, &'a Section)>,
}

fn extract_config_data(cfg: &LoggingConfig) -> ConfigData<'_> {
    let crate_sections = cfg
        .iter()
        .filter(|(k, _)| k.as_str() != "default")
        .map(|(k, v)| (k.clone(), v))
        .collect::<Vec<_>>();

    ConfigData {
        default_section: cfg.get("default"),
        crate_sections,
    }
}

// ================= path helpers =================

fn create_rotating_writer_at_path(
    log_path: &Path,
    max_bytes: usize,
    max_age_days: Option<u32>,
    max_backups: Option<usize>,
) -> Result<RotWriter, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Respect retention policy: prefer MaxFiles if provided, else Age
    let max_age_days = max_age_days.unwrap_or(1);
    let age = chrono::Duration::try_days(i64::from(max_age_days))
        .with_context(|| format!("Invalid max_age_days: {max_age_days}"))?;
    let limit = if let Some(n) = max_backups {
        FileLimit::MaxFiles(n)
    } else {
        FileLimit::Age(age)
    };

    let rot = FileRotate::new(
        log_path,
        AppendTimestamp::default(limit),
        ContentLimit::BytesSurpassed(max_bytes),
        Compression::None,
        None,
    );

    Ok(RotWriter(Arc::new(Mutex::new(rot))))
}

// ================= public init (drop-in API kept) =================

// Stores the `WorkerGuard` for the non-blocking console writer so it is
// never dropped while the process is alive.  Dropping the guard shuts down
// the background flush thread and silently loses buffered log output.
static CONSOLE_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Unified initializer used by both functions above.
#[allow(unknown_lints, de1301_no_print_macros)] // runs before tracing subscriber is installed
pub fn init_logging_unified(
    cfg: &LoggingConfig,
    base_dir: &Path,
    otel_layer: Option<OtelLayer>,
    inject_trace_ids: bool,
) {
    CONSOLE_GUARD.get_or_init(|| {
        // Bridge `log` → `tracing` *before* installing the subscriber
        if let Err(e) = tracing_log::LogTracer::init() {
            eprintln!("LogTracer init skipped: {e}");
        }

        let data = extract_config_data(cfg);

        if data.crate_sections.is_empty() && data.default_section.is_none() {
            // Minimal fallback (INFO to console; honors RUST_LOG)
            return init_minimal(otel_layer);
        }

        // Build targets once, using a generic builder for different sinks
        let file_router = build_file_router(&data, base_dir);

        let console_targets = build_target_console(&data);
        let file_targets = build_target_file(&data, file_router.default.is_some());

        let console_format = data
            .default_section
            .map(|s| s.console_format)
            .unwrap_or_default();

        install_subscriber(
            &console_targets,
            &file_targets,
            file_router,
            console_format,
            otel_layer,
            inject_trace_ids,
        )
    });
}

// ================= generic targets builder =================

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::Targets;

/// Noisy crates that should be filtered to WARN level to avoid debug spam
const NOISY_CRATES: &[&str] = &["h2"];

fn build_target_console(config: &ConfigData) -> Targets {
    // default level
    let default_level = config
        .default_section
        .and_then(|s| s.console_level)
        .map_or(LevelFilter::INFO, LevelFilter::from_level);

    // start with default
    let mut targets = Targets::new().with_default(default_level);

    // Suppress noisy low-level crates to WARN unless they need DEBUG/TRACE
    for crate_name in NOISY_CRATES {
        targets = targets.with_target(*crate_name, LevelFilter::WARN);
    }

    // per-crate rules (console sink is always "active")
    for (crate_name, section) in &config.crate_sections {
        if let Some(level) = section.console_level.map(LevelFilter::from_level) {
            targets = targets.with_target(crate_name.clone(), level);
        }
    }

    targets
}

fn build_target_file(config: &ConfigData, has_default_file: bool) -> Targets {
    // default level depends on whether there is a default file sink
    let default_level = if has_default_file {
        config
            .default_section
            .and_then(Section::file_level)
            .map_or(LevelFilter::INFO, LevelFilter::from_level)
    } else {
        LevelFilter::OFF
    };

    let mut targets = Targets::new().with_default(default_level);

    // per-crate rules: file sink is "active" only when path is present
    for (crate_name, section) in &config.crate_sections {
        if let Some(level) = section.file_level().map(LevelFilter::from_level) {
            targets = targets.with_target(crate_name.clone(), level);
        }
    }

    targets
}

// ================= building routers =================

fn build_file_router(config: &ConfigData, base_dir: &Path) -> MultiFileRouter {
    let mut router = MultiFileRouter {
        default: None,
        by_prefix: Vec::with_capacity(config.crate_sections.len()),
    };

    if let Some(section) = config.default_section {
        router.default = create_file_writer(None, section, base_dir);
    }

    for (crate_name, section) in &config.crate_sections {
        if let Some(writer) = create_file_writer(Some(crate_name), section, base_dir) {
            router.by_prefix.push((crate_name.clone(), writer));
        }
    }

    // Sort by descending prefix length so longest (most specific) match wins.
    // Ties broken lexicographically for determinism.
    router
        .by_prefix
        .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));

    router
}

trait HasMaxSizeBytes {
    fn max_size_bytes(&self) -> usize;
}

const DEFAULT_SECTION_MAX_SIZE_MB: usize = 100;

impl HasMaxSizeBytes for Section {
    fn max_size_bytes(&self) -> usize {
        self.max_size_mb
            .map(|mb| mb * 1024 * 1024)
            .and_then(|b| usize::try_from(b).ok())
            .unwrap_or(DEFAULT_SECTION_MAX_SIZE_MB * 1024 * 1024)
    }
}

#[allow(unknown_lints, de1301_no_print_macros)] // runs during logging init, before tracing is available
fn create_file_writer(
    crate_name: Option<&str>,
    section: &Section,
    base_dir: &Path,
) -> Option<RotWriter> {
    let file = section.file()?;

    let max_bytes = section.max_size_bytes();

    let p = Path::new(file);
    let log_path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    };

    match create_rotating_writer_at_path(
        &log_path,
        max_bytes,
        section.max_age_days,
        section.max_backups,
    ) {
        Ok(writer) => Some(writer),
        Err(e) => {
            match crate_name {
                Some(crate_name) => eprintln!(
                    "Failed to init log file for subsystem '{}': {} ({})",
                    crate_name,
                    log_path.to_string_lossy(),
                    e,
                ),
                None => eprintln!(
                    "Failed to initialize default log file '{}'",
                    log_path.to_string_lossy()
                ),
            }
            None
        }
    }
}

// ================= ANSI color support =================

/// Returns `true` if stderr supports ANSI color escape codes.
/// On Windows, also attempts to enable virtual-terminal color processing.
fn stderr_supports_ansi() -> bool {
    _ = enable_ansi_support::enable_ansi_support();
    supports_color::on(supports_color::Stream::Stderr).is_some_and(|level| level.has_basic)
}

// ================= registry & layers =================

// Keep a guard for non-blocking console to avoid being dropped.

// de1301_no_print_macros: eprintln! is intentional here — if the tracing subscriber
// fails to initialize we cannot use tracing itself to report the failure.
#[allow(unknown_lints, de1301_no_print_macros)]
fn install_subscriber(
    console_targets: &tracing_subscriber::filter::Targets,
    file_targets: &tracing_subscriber::filter::Targets,
    file_router: MultiFileRouter,
    console_format: ConsoleFormat,
    #[cfg_attr(not(feature = "otel"), allow(unused_variables))] otel_layer: Option<OtelLayer>,
    #[cfg_attr(not(feature = "otel"), allow(unused_variables))] inject_trace_ids: bool,
) -> WorkerGuard {
    use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};

    // RUST_LOG acts as a global upper-bound for console/file if present.
    // If not set, we don't clamp here — YAML targets drive levels.
    let env: Option<EnvFilter> = EnvFilter::try_from_default_env().ok();

    // Console writer (non-blocking stderr)
    let (nb_stderr, guard) = tracing_appender::non_blocking(std::io::stderr());

    // Shared base JSON `Format`, referenced by the doc comment on
    // `JsonEventFormat` above. Built once here so the console and file JSON
    // sinks cannot drift from each other.
    let base_json_format = || {
        fmt::format()
            .json()
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_timer(fmt::time::UtcTime::rfc_3339())
    };

    // Console fmt layers: text (human-friendly) or JSON (structured).
    // Only one is active at a time; the other is None.
    let (console_text, console_json) = match console_format {
        ConsoleFormat::Text => (
            Some(
                fmt::layer()
                    .with_writer(nb_stderr)
                    .with_ansi(stderr_supports_ansi())
                    .with_target(true)
                    .with_level(true)
                    .with_timer(fmt::time::UtcTime::rfc_3339())
                    .with_filter(console_targets.clone()),
            ),
            None,
        ),
        ConsoleFormat::Json => (
            None,
            Some(
                fmt::layer()
                    .json()
                    .event_format(json_event_format(base_json_format(), inject_trace_ids))
                    .with_writer(nb_stderr)
                    .with_filter(console_targets.clone()),
            ),
        ),
    };

    // File fmt layer (JSON) if router is not empty
    let file_layer_opt = if file_router.is_empty() {
        None
    } else {
        Some(
            fmt::layer()
                .json()
                .event_format(json_event_format(base_json_format(), inject_trace_ids))
                .with_writer(file_router)
                .with_filter(file_targets.clone()),
        )
    };

    // Build subscriber:
    // 1) OTEL first (because your OtelLayer is bound to `Registry`);
    //    also filter OTEL by the SAME console targets from YAML.
    // 2) Then EnvFilter (caps console/file if RUST_LOG is set).
    // 3) Then console (text or json) + file fmt layers.
    let subscriber = {
        let base = Registry::default();

        #[cfg(feature = "otel")]
        let base = {
            let otel_opt = otel_layer.map(|otel| otel.with_filter(console_targets.clone()));
            base.with(otel_opt)
        };
        #[cfg(not(feature = "otel"))]
        let base = base;

        let base = base.with(env);
        base.with(console_text)
            .with(console_json)
            .with(file_layer_opt)
    };

    if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("tracing subscriber init failed: {e}");
    }

    guard
}
// de1301_no_print_macros: same rationale as install_subscriber above.
#[allow(unknown_lints, de1301_no_print_macros)]
/// Minimal fallback subscriber: INFO to the console, honouring `RUST_LOG`.
///
/// Text output only, so trace-id injection does not apply here — it adds fields
/// to JSON records.
fn init_minimal(
    #[cfg_attr(not(feature = "otel"), allow(unused_variables))] otel: Option<OtelLayer>,
) -> WorkerGuard {
    use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};

    // If RUST_LOG is set, it will cap fmt output; otherwise don't clamp here.
    let env = EnvFilter::try_from_default_env().ok();

    // Console writer (non-blocking stderr)
    let (nb_stderr, guard) = tracing_appender::non_blocking(std::io::stderr());

    let fmt_layer = fmt::layer()
        .with_writer(nb_stderr)
        .with_ansi(stderr_supports_ansi())
        .with_target(true)
        .with_timer(fmt::time::UtcTime::rfc_3339());

    // Same ordering: OTEL (if any) first, then EnvFilter, then fmt.
    let subscriber = {
        let base = Registry::default();

        #[cfg(feature = "otel")]
        let base = base.with(otel);
        #[cfg(not(feature = "otel"))]
        let base = base;

        base.with(env).with(fmt_layer)
    };

    // LogTracer is already initialized by the caller (init_logging_unified),
    // so use set_global_default instead of try_init to avoid double log::set_logger.
    if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("tracing subscriber init failed (minimal): {e}");
    }

    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: run concurrent write test through a given `MakeWriter` and verify output.
    fn assert_concurrent_writes<'a, W>(writer: &'a W, log_path: &Path)
    where
        W: fmt::MakeWriter<'a> + Sync,
        W::Writer: Write,
    {
        const NUM_THREADS: usize = 8;
        const LINES_PER_THREAD: usize = 500;
        const TOTAL_LINES: usize = NUM_THREADS * LINES_PER_THREAD;

        std::thread::scope(|s| {
            for thread_id in 0..NUM_THREADS {
                s.spawn(move || {
                    for line_no in 0..LINES_PER_THREAD {
                        let mut handle = writer.make_writer();
                        writeln!(handle, "thread={thread_id} line={line_no}")
                            .expect("write must not fail");
                    }
                });
            }
        });

        let content = std::fs::read_to_string(log_path).expect("failed to read log file");
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(
            lines.len(),
            TOTAL_LINES,
            "expected {TOTAL_LINES} lines but found {} ({} records {})",
            lines.len(),
            lines.len().abs_diff(TOTAL_LINES),
            if lines.len() < TOTAL_LINES {
                "lost"
            } else {
                "extra"
            },
        );

        // Verify every line is intact (no interleaved bytes)
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.starts_with("thread=") && line.contains(" line="),
                "corrupted line {i}: {line:?}",
            );
        }
    }

    #[test]
    fn concurrent_writes_are_not_dropped() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let log_path = dir.path().join("test.log");

        let writer = create_rotating_writer_at_path(&log_path, 50 * 1024 * 1024, None, Some(1))
            .expect("failed to create rotating writer");

        assert_concurrent_writes(&writer, &log_path);
    }

    #[test]
    fn concurrent_writes_through_routed_writer() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let log_path = dir.path().join("routed.log");

        let rot = create_rotating_writer_at_path(&log_path, 50 * 1024 * 1024, None, Some(1))
            .expect("failed to create rotating writer");

        let router = MultiFileRouter {
            default: Some(rot),
            by_prefix: Vec::new(),
        };

        assert_concurrent_writes(&router, &log_path);
    }

    /// Helper: create a `RotWriter` for a temp path and return (writer, path).
    fn tmp_writer(dir: &Path, name: &str) -> (RotWriter, std::path::PathBuf) {
        let p = dir.join(name);
        let w = create_rotating_writer_at_path(&p, 50 * 1024 * 1024, None, Some(1))
            .expect("failed to create rotating writer");
        (w, p)
    }

    #[test]
    fn resolve_for_picks_longest_matching_prefix() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let (broad, broad_path) = tmp_writer(dir.path(), "broad.log");
        let (specific, specific_path) = tmp_writer(dir.path(), "specific.log");

        // Write markers so we can tell them apart
        broad.0.lock().write_all(b"BROAD\n").unwrap();
        specific.0.lock().write_all(b"SPECIFIC\n").unwrap();

        let router = MultiFileRouter {
            default: None,
            by_prefix: vec![
                ("cf-gears::api_gateway".into(), specific),
                ("cf-gears".into(), broad),
            ],
        };

        // "cf-gears::api_gateway::handler" should match the longer prefix
        let mut handle = router
            .resolve_for("cf-gears::api_gateway::handler")
            .expect("should resolve");
        handle.write_all(b"routed\n").unwrap();
        handle.flush().unwrap();

        let specific_content = std::fs::read_to_string(&specific_path).unwrap();
        assert!(
            specific_content.contains("routed"),
            "expected write to land in specific log, got: {specific_content:?}"
        );

        let broad_content = std::fs::read_to_string(&broad_path).unwrap();
        assert!(
            !broad_content.contains("routed"),
            "write should NOT appear in broad log, got: {broad_content:?}"
        );
    }

    /// Verifies that `build_file_router` sorts `by_prefix` by descending length so
    /// that the longest (most-specific) prefix wins even when the caller registers
    /// a broad prefix before a specific one.
    #[test]
    fn build_file_router_sorts_prefixes_longest_match_wins() {
        use crate::bootstrap::config::SectionFile;

        let dir = tempfile::tempdir().expect("failed to create temp dir");

        let broad_section = Section {
            console_format: ConsoleFormat::default(),
            console_level: None,
            section_file: Some(SectionFile {
                file: "broad.log".to_owned(),
                file_level: None,
            }),
            max_age_days: None,
            max_backups: Some(1),
            max_size_mb: None,
        };
        let specific_section = Section {
            console_format: ConsoleFormat::default(),
            console_level: None,
            section_file: Some(SectionFile {
                file: "specific.log".to_owned(),
                file_level: None,
            }),
            max_age_days: None,
            max_backups: Some(1),
            max_size_mb: None,
        };

        // Register broad BEFORE specific (reverse of preference order) so that
        // build_file_router's sort step is what makes the specific prefix win.
        let config = ConfigData {
            default_section: None,
            crate_sections: vec![
                ("cf-gears".to_owned(), &broad_section),
                ("cf-gears::api_gateway".to_owned(), &specific_section),
            ],
        };

        let router = build_file_router(&config, dir.path());

        let mut handle = router
            .resolve_for("cf-gears::api_gateway::handler")
            .expect("should resolve");
        handle.write_all(b"routed\n").unwrap();
        handle.flush().unwrap();

        let specific_content = std::fs::read_to_string(dir.path().join("specific.log")).unwrap();
        assert!(
            specific_content.contains("routed"),
            "expected write to land in specific log, got: {specific_content:?}"
        );

        let broad_content = std::fs::read_to_string(dir.path().join("broad.log")).unwrap();
        assert!(
            !broad_content.contains("routed"),
            "write should NOT appear in broad log, got: {broad_content:?}"
        );
    }

    #[test]
    fn resolve_for_exact_match() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let (writer, _) = tmp_writer(dir.path(), "exact.log");

        let router = MultiFileRouter {
            default: None,
            by_prefix: vec![("cf-gears".into(), writer)],
        };

        // Exact match
        assert!(
            router.resolve_for("cf-gears").is_some(),
            "exact target should match"
        );
        // Subgear match
        assert!(
            router.resolve_for("cf-gears::sub").is_some(),
            "submodule target should match"
        );
        // Non-prefix string must NOT match
        assert!(
            router.resolve_for("cf_gears_extra").is_none(),
            "non-prefix target should not match"
        );
        assert!(
            router.resolve_for("other").is_none(),
            "unrelated target should not match"
        );
    }

    #[test]
    fn resolve_for_falls_back_to_default() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let (default_writer, default_path) = tmp_writer(dir.path(), "default.log");

        default_writer.0.lock().write_all(b"DEFAULT\n").unwrap();

        let router = MultiFileRouter {
            default: Some(default_writer),
            by_prefix: vec![],
        };

        // Unknown target should fall back to default
        let mut handle = router
            .resolve_for("unknown_crate::gear")
            .expect("should fall back to default");
        handle.write_all(b"fallback\n").unwrap();
        handle.flush().unwrap();

        let content = std::fs::read_to_string(&default_path).unwrap();
        assert!(
            content.contains("fallback"),
            "expected write to land in default log, got: {content:?}"
        );
    }
}
