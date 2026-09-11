//! OpenTelemetry tracing initialization utilities
//!
//! This gear sets up OpenTelemetry tracing and exports spans via OTLP
//! (gRPC or HTTP) to collectors such as Jaeger, Uptrace, or the `OTel` Collector.

#[cfg(feature = "otel")]
use anyhow::Context;
#[cfg(feature = "otel")]
use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
#[cfg(feature = "otel")]
use std::sync::Once;

#[cfg(feature = "otel")]
use opentelemetry_otlp::{Protocol, WithExportConfig};
// Bring extension traits into scope for builder methods like `.with_headers()` and `.with_metadata()`.
#[cfg(feature = "otel")]
use opentelemetry_otlp::{WithHttpConfig, WithTonicConfig};

#[cfg(feature = "otel")]
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{Sampler, SdkTracerProvider},
};

#[cfg(feature = "otel")]
use super::config::{OpenTelemetryConfig, OpenTelemetryResource, TracingConfig};
#[cfg(feature = "otel")]
use crate::telemetry::config::ExporterKind;
#[cfg(feature = "otel")]
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};

// ===== init_tracing (feature = "otel") ========================================

/// Build resource with service name and custom attributes
#[cfg(feature = "otel")]
pub(crate) fn build_resource(cfg: &OpenTelemetryResource) -> Resource {
    tracing::debug!(
        "Building OpenTelemetry resource for service: {}",
        cfg.service_name
    );
    let mut attrs = vec![KeyValue::new("service.name", cfg.service_name.clone())];
    if let Some(version) = cfg.effective_service_version() {
        attrs.push(KeyValue::new("service.version", version.to_owned()));
    }

    for (k, v) in &cfg.attributes {
        // Skip caller-supplied "service.name" / "service.version" entries: the
        // dedicated seeds above already carry them and a duplicate key would
        // create ambiguity in the resource attributes.
        if k == "service.name" || k == "service.version" {
            continue;
        }
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }

    Resource::builder_empty().with_attributes(attrs).build()
}

/// Build sampler from configuration
#[cfg(feature = "otel")]
fn build_sampler(cfg: &TracingConfig) -> Sampler {
    match cfg.sampler.as_ref() {
        Some(crate::telemetry::config::Sampler::AlwaysOff { .. }) => Sampler::AlwaysOff,
        Some(crate::telemetry::config::Sampler::AlwaysOn { .. }) => Sampler::AlwaysOn,
        Some(crate::telemetry::config::Sampler::ParentBasedAlwaysOn { .. }) => {
            Sampler::ParentBased(Box::new(Sampler::AlwaysOn))
        }
        Some(crate::telemetry::config::Sampler::ParentBasedRatio { ratio }) => {
            let ratio = ratio.unwrap_or(0.1);
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio)))
        }
        None => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
    }
}

/// Extract exporter kind and endpoint from the resolved exporter.
#[cfg(feature = "otel")]
pub(crate) fn extract_exporter_config(
    exporter: Option<&crate::telemetry::config::Exporter>,
) -> (ExporterKind, String, Option<std::time::Duration>) {
    let kind = exporter.map_or(ExporterKind::OtlpGrpc, |e| e.kind);
    let default_endpoint = match kind {
        ExporterKind::OtlpHttp => "http://127.0.0.1:4318",
        ExporterKind::OtlpGrpc => "http://127.0.0.1:4317",
    };
    let endpoint = exporter
        .and_then(|e| e.endpoint.clone())
        .unwrap_or_else(|| default_endpoint.into());

    let timeout = exporter
        .and_then(|e| e.timeout_ms)
        .map(std::time::Duration::from_millis);

    (kind, endpoint, timeout)
}

/// Build HTTP OTLP exporter
#[cfg(feature = "otel")]
fn build_http_exporter(
    exporter: Option<&crate::telemetry::config::Exporter>,
    endpoint: String,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<opentelemetry_otlp::SpanExporter> {
    let mut b = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint);
    if let Some(t) = timeout {
        b = b.with_timeout(t);
    }
    if let Some(hmap) = build_headers_from_cfg_and_env(exporter) {
        b = b.with_headers(hmap);
    }
    b.build().context("build OTLP HTTP exporter")
}

/// Build gRPC OTLP exporter
#[cfg(feature = "otel")]
fn build_grpc_exporter(
    exporter: Option<&crate::telemetry::config::Exporter>,
    endpoint: String,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<opentelemetry_otlp::SpanExporter> {
    let mut b = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint);
    if let Some(t) = timeout {
        b = b.with_timeout(t);
    }
    if let Some(md) = build_metadata_from_cfg_and_env(exporter) {
        b = b.with_metadata(md);
    }
    b.build().context("build OTLP gRPC exporter")
}

#[cfg(feature = "otel")]
static INIT_TRACING: Once = Once::new();

/// Initialize OpenTelemetry tracing from configuration and return a layer
/// to be attached to `tracing_subscriber`.
///
/// # Errors
/// Returns an error if the configuration is invalid or if the exporter fails to build.
#[cfg(feature = "otel")]
pub fn init_tracing(
    otel_cfg: &OpenTelemetryConfig,
) -> anyhow::Result<
    tracing_opentelemetry::OpenTelemetryLayer<
        tracing_subscriber::Registry,
        opentelemetry_sdk::trace::Tracer,
    >,
> {
    let cfg = &otel_cfg.tracing;
    if !cfg.enabled {
        return Err(anyhow::anyhow!("tracing is disabled"));
    }

    // Set W3C propagator for trace-context propagation
    global::set_text_map_propagator(TraceContextPropagator::new());

    // Build resource, sampler, and extract exporter config
    let resource = build_resource(&otel_cfg.resource);
    let sampler = build_sampler(cfg);
    let resolved_exporter = otel_cfg.tracing_exporter();
    let (kind, endpoint, timeout) = extract_exporter_config(resolved_exporter);

    tracing::info!(kind = ?kind, %endpoint, "OTLP exporter config");

    // Build span exporter based on kind
    let exporter = if matches!(kind, ExporterKind::OtlpHttp) {
        build_http_exporter(resolved_exporter, endpoint, timeout)
    } else {
        build_grpc_exporter(resolved_exporter, endpoint, timeout)
    }?;

    // Build tracer provider with batch processor
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    // Create tracer and layer
    let service_name = otel_cfg.resource.service_name.clone();
    let tracer = provider.tracer(service_name);
    let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(tracer);

    // Make it global
    INIT_TRACING.call_once(|| {
        global::set_tracer_provider(provider);
    });

    tracing::info!("OpenTelemetry layer created successfully");
    Ok(otel_layer)
}

#[cfg(feature = "otel")]
pub(crate) fn build_headers_from_cfg_and_env(
    exporter: Option<&crate::telemetry::config::Exporter>,
) -> Option<std::collections::HashMap<String, String>> {
    use std::collections::HashMap;
    let mut out: HashMap<String, String> = HashMap::new();

    // From config file
    if let Some(exp) = exporter
        && let Some(hdrs) = &exp.headers
    {
        for (k, v) in hdrs {
            out.insert(k.clone(), v.clone());
        }
    }

    // From ENV OTEL_EXPORTER_OTLP_HEADERS (format: k=v,k2=v2)
    if let Ok(env_hdrs) = std::env::var("OTEL_EXPORTER_OTLP_HEADERS") {
        for part in env_hdrs.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some((k, v)) = part.split_once('=') {
                out.insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
    }

    if out.is_empty() { None } else { Some(out) }
}

#[cfg(feature = "otel")]
pub(crate) fn extend_metadata_from_source<'a, I>(
    md: &mut MetadataMap,
    source: I,
    context: &'static str,
) where
    I: Iterator<Item = (&'a str, &'a str)>,
{
    for (k, v) in source {
        match MetadataKey::from_bytes(k.as_bytes()) {
            Ok(key) => match MetadataValue::try_from(v) {
                Ok(val) => {
                    md.insert(key, val);
                }
                Err(_) => {
                    tracing::warn!(header = %k, context, "Skipping invalid gRPC metadata value");
                }
            },
            Err(_) => {
                tracing::warn!(header = %k, context, "Skipping invalid gRPC metadata header name");
            }
        }
    }
}

#[cfg(feature = "otel")]
pub(crate) fn build_metadata_from_cfg_and_env(
    exporter: Option<&crate::telemetry::config::Exporter>,
) -> Option<MetadataMap> {
    let mut md = MetadataMap::new();

    // From config file
    if let Some(exp) = exporter
        && let Some(hdrs) = &exp.headers
    {
        let iter = hdrs.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        extend_metadata_from_source(&mut md, iter, "config");
    }

    // From ENV OTEL_EXPORTER_OTLP_HEADERS (format: k=v,k2=v2)
    if let Ok(env_hdrs) = std::env::var("OTEL_EXPORTER_OTLP_HEADERS") {
        let iter = env_hdrs.split(',').filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                None
            } else {
                part.split_once('=').map(|(k, v)| (k.trim(), v.trim()))
            }
        });
        extend_metadata_from_source(&mut md, iter, "env");
    }

    if md.is_empty() { None } else { Some(md) }
}

// ===== shutdown_tracing =======================================================

/// Gracefully shut down OpenTelemetry tracing.
/// In opentelemetry 0.31 there is no global `shutdown_tracer_provider()`.
/// Keep a handle to `SdkTracerProvider` in your app state and call `shutdown()`
/// during graceful shutdown. This function remains a no-op for compatibility.
#[cfg(feature = "otel")]
pub fn shutdown_tracing() {
    tracing::info!("Tracing shutdown: no-op (keep a provider handle to call `shutdown()`).");
}

#[cfg(not(feature = "otel"))]
pub fn shutdown_tracing() {
    tracing::info!("Tracing shutdown (no-op)");
}

/// Gracefully shut down OpenTelemetry metrics.
/// In opentelemetry 0.31 there is no global `shutdown_meter_provider()`.
/// Keep a handle to `SdkMeterProvider` in your app state and call `shutdown()`
/// during graceful shutdown. This function remains a no-op for compatibility.
#[cfg(feature = "otel")]
pub fn shutdown_metrics() {
    tracing::info!("Metrics shutdown: no-op (keep a provider handle to call `shutdown()`).");
}

#[cfg(not(feature = "otel"))]
pub fn shutdown_metrics() {
    tracing::info!("Metrics shutdown (no-op)");
}

// ===== init_metrics_provider ==================================================

#[cfg(feature = "otel")]
static METRICS_INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Build a [`SdkMeterProvider`] from the resolved metrics exporter settings and
/// register it as the global meter provider.
///
/// When `metrics.enabled` is `false` the function is a no-op: the global meter
/// provider stays as the built-in [`NoopMeterProvider`] (zero overhead — all
/// instruments obtained via `global::meter_with_scope()` become no-op).
///
/// Exporter resolution: `opentelemetry.metrics.exporter` overrides
/// `opentelemetry.exporter` when present.
///
/// Initialisation runs at most once on success and is cached. Errors are
/// **not** cached — a transient failure (e.g. OTLP endpoint briefly
/// unreachable) is returned to the caller, and a subsequent call may retry.
///
/// # Errors
///
/// The OTLP metric exporter cannot be constructed.
#[cfg(feature = "otel")]
pub fn init_metrics_provider(otel_cfg: &OpenTelemetryConfig) -> anyhow::Result<()> {
    if !otel_cfg.metrics.enabled {
        // Do NOT cache the disabled path in METRICS_INIT — a later call with
        // metrics enabled must still be able to initialise the real provider.
        tracing::info!(
            "OpenTelemetry metrics disabled - global meter provider is \
             the built-in NoopMeterProvider"
        );
        return Ok(());
    }

    if METRICS_INIT.get().is_some() {
        return Ok(());
    }

    do_init_metrics_provider(otel_cfg)?;
    // Race between concurrent first-callers is benign: OTel allows
    // re-registering the global meter provider, so at worst we initialise
    // twice. The `set` returns Err for the loser of the race; ignored.
    let _set_result = METRICS_INIT.set(());
    Ok(())
}

#[cfg(feature = "otel")]
fn do_init_metrics_provider(otel_cfg: &OpenTelemetryConfig) -> anyhow::Result<()> {
    let resolved_exporter = otel_cfg.metrics_exporter();

    let (kind, endpoint, timeout) = extract_exporter_config(resolved_exporter);

    // Build OTLP metric exporter matching the configured transport
    let exporter = if matches!(kind, ExporterKind::OtlpHttp) {
        let mut b = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(&endpoint);
        if let Some(t) = timeout {
            b = b.with_timeout(t);
        }
        if let Some(headers) = build_headers_from_cfg_and_env(resolved_exporter) {
            b = b.with_headers(headers);
        }
        b.build().context("build OTLP HTTP metric exporter")?
    } else {
        let mut b = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(&endpoint);
        if let Some(t) = timeout {
            b = b.with_timeout(t);
        }
        if let Some(md) = build_metadata_from_cfg_and_env(resolved_exporter) {
            b = b.with_metadata(md);
        }
        b.build().context("build OTLP gRPC metric exporter")?
    };

    // Build resource with service name and attributes
    let resource = build_resource(&otel_cfg.resource);

    // Build the SdkMeterProvider with periodic exporter
    let mut builder = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_periodic_exporter(exporter)
        .with_resource(resource);

    // Apply a global cardinality limit when configured
    if let Some(limit) = otel_cfg.metrics.cardinality_limit {
        builder = builder.with_view(move |_: &opentelemetry_sdk::metrics::Instrument| {
            opentelemetry_sdk::metrics::Stream::builder()
                .with_cardinality_limit(limit)
                .build()
                .ok()
        });
    }

    let provider = builder.build();

    global::set_meter_provider(provider);
    tracing::info!("OpenTelemetry metrics initialized successfully");

    Ok(())
}

/// No-op when the `otel` feature is disabled.
///
/// # Errors
/// Always returns an error indicating the feature is disabled.
#[cfg(not(feature = "otel"))]
pub fn init_metrics_provider(_otel_cfg: &super::config::OpenTelemetryConfig) -> anyhow::Result<()> {
    Err(anyhow::anyhow!("otel feature is disabled"))
}

// ===== connectivity probe =====================================================

/// Build a tiny, separate OTLP pipeline and export a single span to verify connectivity.
/// This does *not* depend on `tracing_subscriber`; it uses SDK directly.
///
/// # Errors
/// Returns an error if the OTLP exporter cannot be built or the probe fails.
#[cfg(feature = "otel")]
pub fn otel_connectivity_probe(otel_cfg: &OpenTelemetryConfig) -> anyhow::Result<()> {
    use opentelemetry::trace::{Span, Tracer as _};

    let resolved_exporter = otel_cfg.tracing_exporter();
    let (kind, endpoint, timeout) = extract_exporter_config(resolved_exporter);

    // Resource (reuse shared builder)
    let resource = build_resource(&otel_cfg.resource);

    // Exporter (type-state branches again)
    let exporter = if matches!(kind, ExporterKind::OtlpHttp) {
        let mut b = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint);
        if let Some(t) = timeout {
            b = b.with_timeout(t);
        }
        if let Some(h) = build_headers_from_cfg_and_env(resolved_exporter) {
            b = b.with_headers(h);
        }
        b.build()
            .map_err(|e| anyhow::anyhow!("otlp http exporter build failed: {e}"))?
    } else {
        let mut b = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint);
        if let Some(t) = timeout {
            b = b.with_timeout(t);
        }
        if let Some(md) = build_metadata_from_cfg_and_env(resolved_exporter) {
            b = b.with_metadata(md);
        }
        b.build()
            .map_err(|e| anyhow::anyhow!("otlp grpc exporter build failed: {e}"))?
    };

    // Provider (simple processor is fine for a probe)
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter)
        .with_resource(resource)
        .build();

    // Emit a single span
    let tracer = provider.tracer("connectivity_probe");
    let mut span = tracer.start("otel_connectivity_probe");
    span.end();

    // Ensure delivery
    if let Err(e) = provider.force_flush() {
        tracing::warn!(error = %e, "force_flush failed during OTLP connectivity probe");
    }

    provider
        .shutdown()
        .map_err(|e| anyhow::anyhow!("shutdown failed: {e}"))?;

    tracing::info!(kind = ?kind, "OTLP connectivity probe exported a test span");
    Ok(())
}

/// OTLP connectivity probe (no-op when otel feature is disabled).
///
/// # Errors
/// This function always succeeds when the otel feature is disabled.
#[cfg(not(feature = "otel"))]
pub fn otel_connectivity_probe(_cfg: &super::config::OpenTelemetryConfig) -> anyhow::Result<()> {
    tracing::info!("OTLP connectivity probe skipped (otel feature disabled)");
    Ok(())
}

// ===== tests ==================================================================

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    #[cfg(feature = "otel")]
    use crate::telemetry::config::{
        Exporter, ExporterKind, OpenTelemetryConfig, OpenTelemetryResource, Sampler, TracingConfig,
    };
    #[cfg(feature = "otel")]
    use std::collections::{BTreeMap, HashMap};

    /// Helper to build an `OpenTelemetryConfig` with the given tracing config.
    #[cfg(feature = "otel")]
    fn otel_with_tracing(tracing: TracingConfig) -> OpenTelemetryConfig {
        OpenTelemetryConfig {
            tracing,
            ..Default::default()
        }
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_disabled() {
        let otel = otel_with_tracing(TracingConfig {
            enabled: false,
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_err());
    }

    #[tokio::test]
    #[cfg(feature = "otel")]
    async fn test_init_tracing_enabled() {
        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn build_resource_carries_the_effective_service_version_once() {
        let mut attrs = BTreeMap::new();
        attrs.insert("service.version".to_owned(), "from-attr".to_owned());

        let from_field = build_resource(&OpenTelemetryResource {
            service_name: "svc".to_owned(),
            service_version: Some("1.2.3".to_owned()),
            attributes: attrs.clone(),
        });
        let from_attr = build_resource(&OpenTelemetryResource {
            service_name: "svc".to_owned(),
            service_version: None,
            attributes: attrs,
        });

        let versions = |resource: &Resource| {
            resource
                .iter()
                .filter(|(k, _)| k.as_str() == "service.version")
                .map(|(_, v)| v.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(versions(&from_field), ["1.2.3"]);
        assert_eq!(versions(&from_attr), ["from-attr"]);
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_resource_attributes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let mut attrs = BTreeMap::new();
        attrs.insert("service.version".to_owned(), "1.0.0".to_owned());
        attrs.insert("deployment.environment".to_owned(), "test".to_owned());

        let otel = OpenTelemetryConfig {
            resource: OpenTelemetryResource {
                service_name: "test-service".to_owned(),
                service_version: None,
                attributes: attrs,
            },
            tracing: TracingConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_always_on_sampler() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            sampler: Some(Sampler::AlwaysOn {}),
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_always_off_sampler() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            sampler: Some(Sampler::AlwaysOff {}),
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_ratio_sampler() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            sampler: Some(Sampler::ParentBasedRatio { ratio: Some(0.5) }),
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_http_exporter() {
        let _rt = tokio::runtime::Runtime::new().unwrap();

        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpHttp,
                endpoint: Some("http://localhost:4318".to_owned()),
                headers: None,
                timeout_ms: Some(5000),
            }),
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_tracing_with_grpc_exporter() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = rt.enter();

        let otel = otel_with_tracing(TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpGrpc,
                endpoint: Some("http://localhost:4317".to_owned()),
                headers: None,
                timeout_ms: Some(5000),
            }),
            ..Default::default()
        });

        let result = init_tracing(&otel);
        assert!(result.is_ok());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_headers_from_cfg_empty() {
        temp_env::with_var_unset("OTEL_EXPORTER_OTLP_HEADERS", || {
            let cfg = TracingConfig {
                enabled: true,
                ..Default::default()
            };

            let result = build_headers_from_cfg_and_env(cfg.exporter.as_ref());
            assert!(
                result.is_none(),
                "expected None when no headers configured and no env"
            );
        });
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_headers_from_cfg_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_owned(), "Bearer token".to_owned());

        let cfg = TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpHttp,
                endpoint: Some("http://localhost:4318".to_owned()),
                headers: Some(headers.clone()),
                timeout_ms: None,
            }),
            ..Default::default()
        };

        let result = build_headers_from_cfg_and_env(cfg.exporter.as_ref());
        assert!(result.is_some());
        let result_headers = result.unwrap();
        assert_eq!(
            result_headers.get("authorization"),
            Some(&"Bearer token".to_owned())
        );
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_metadata_from_cfg_empty() {
        temp_env::with_var_unset("OTEL_EXPORTER_OTLP_HEADERS", || {
            let cfg = TracingConfig {
                enabled: true,
                ..Default::default()
            };

            let result = build_metadata_from_cfg_and_env(cfg.exporter.as_ref());
            assert!(
                result.is_none(),
                "expected None when no headers configured and no env"
            );
        });
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_metadata_from_cfg_with_headers() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_owned(), "Bearer token".to_owned());

        let cfg = TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpGrpc,
                endpoint: Some("http://localhost:4317".to_owned()),
                headers: Some(headers.clone()),
                timeout_ms: None,
            }),
            ..Default::default()
        };

        let result = build_metadata_from_cfg_and_env(cfg.exporter.as_ref());
        assert!(result.is_some());
        let metadata = result.unwrap();
        assert!(!metadata.is_empty());
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_metadata_multiple_headers() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_owned(), "Bearer token".to_owned());
        headers.insert("x-custom-header".to_owned(), "custom-value".to_owned());

        let cfg = TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpGrpc,
                endpoint: Some("http://localhost:4317".to_owned()),
                headers: Some(headers.clone()),
                timeout_ms: None,
            }),
            ..Default::default()
        };

        let result = build_metadata_from_cfg_and_env(cfg.exporter.as_ref());
        assert!(result.is_some());
        let metadata = result.unwrap();
        assert_eq!(metadata.len(), 2);
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_build_metadata_invalid_header_name_skipped() {
        let mut headers = HashMap::new();
        headers.insert("valid-header".to_owned(), "value1".to_owned());
        headers.insert("invalid header with spaces".to_owned(), "value2".to_owned());

        let cfg = TracingConfig {
            enabled: true,
            exporter: Some(Exporter {
                kind: ExporterKind::OtlpGrpc,
                endpoint: Some("http://localhost:4317".to_owned()),
                headers: Some(headers.clone()),
                timeout_ms: None,
            }),
            ..Default::default()
        };

        let result = build_metadata_from_cfg_and_env(cfg.exporter.as_ref());
        assert!(result.is_some());
        let metadata = result.unwrap();
        // Should only have the valid header
        assert_eq!(metadata.len(), 1);
    }

    #[test]
    fn test_shutdown_tracing_does_not_panic() {
        // Should not panic regardless of feature state
        shutdown_tracing();
    }

    #[test]
    #[cfg(feature = "otel")]
    fn test_init_metrics_provider_disabled() {
        let otel = OpenTelemetryConfig {
            metrics: crate::telemetry::config::MetricsConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        // Disabled path returns Ok (noop — global provider stays NoopMeterProvider)
        let result = init_metrics_provider(&otel);
        assert!(result.is_ok());
    }
}
