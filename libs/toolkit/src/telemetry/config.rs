//! OpenTelemetry tracing and metrics configuration types
//!
//! These types define the configuration structure for OpenTelemetry distributed
//! tracing and metrics.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Top-level OpenTelemetry configuration grouping resource identity,
/// a shared default exporter, tracing settings and metrics settings.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OpenTelemetryConfig {
    #[serde(default)]
    pub resource: OpenTelemetryResource,
    /// Default exporter shared by tracing and metrics. Per-signal `exporter`
    /// fields override this when present.
    pub exporter: Option<Exporter>,
    #[serde(default)]
    pub tracing: TracingConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

impl OpenTelemetryConfig {
    /// Resolve the effective exporter for tracing (per-signal or shared fallback).
    #[must_use]
    pub fn tracing_exporter(&self) -> Option<&Exporter> {
        self.tracing.exporter.as_ref().or(self.exporter.as_ref())
    }
    /// Resolve the effective exporter for metrics (per-signal or shared fallback).
    #[must_use]
    pub fn metrics_exporter(&self) -> Option<&Exporter> {
        self.metrics.exporter.as_ref().or(self.exporter.as_ref())
    }
}

/// OpenTelemetry resource identity — attached to all traces and metrics.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTelemetryResource {
    /// Logical service name.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// Release version (`service.version` semconv); also on every JSON log line.
    #[serde(default)]
    pub service_version: Option<String>,
    /// Extra resource attributes added to every span and metric data point.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Return the default OpenTelemetry service name used when none is configured.
fn default_service_name() -> String {
    "cf-gears".to_owned()
}

impl OpenTelemetryResource {
    /// `service_version`, falling back to an `attributes["service.version"]` entry.
    #[must_use]
    pub fn effective_service_version(&self) -> Option<&str> {
        self.service_version
            .as_deref()
            .or_else(|| self.attributes.get("service.version").map(String::as_str))
    }
}

impl Default for OpenTelemetryResource {
    fn default() -> Self {
        Self {
            service_name: default_service_name(),
            service_version: None,
            attributes: BTreeMap::default(),
        }
    }
}

/// Tracing configuration for OpenTelemetry distributed tracing
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TracingConfig {
    pub enabled: bool,
    /// Per-signal exporter override. When `None`, the shared
    /// [`OpenTelemetryConfig::exporter`] is used instead.
    pub exporter: Option<Exporter>,
    pub sampler: Option<Sampler>,
    pub propagation: Option<Propagation>,
    pub http: Option<HttpOpts>,
    pub logs_correlation: Option<LogsCorrelation>,
}

/// Metrics configuration for OpenTelemetry metrics collection
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    /// Per-signal exporter override. When `None`, the shared
    /// [`OpenTelemetryConfig::exporter`] is used instead.
    pub exporter: Option<Exporter>,
    /// Maximum number of distinct attribute combinations per instrument.
    /// When the limit is reached, new combinations are folded into an
    /// overflow data point.  `None` means the SDK default is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cardinality_limit: Option<usize>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExporterKind {
    #[default]
    OtlpGrpc,
    OtlpHttp,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Exporter {
    pub kind: ExporterKind,
    pub endpoint: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Sampler {
    ParentBasedAlwaysOn {},
    ParentBasedRatio {
        #[serde(skip_serializing_if = "Option::is_none")]
        ratio: Option<f64>,
    },
    AlwaysOn {},
    AlwaysOff {},
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Propagation {
    pub w3c_trace_context: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpOpts {
    pub inject_request_id_header: Option<String>,
    pub record_headers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogsCorrelation {
    pub inject_trace_ids_into_logs: Option<bool>,
}
