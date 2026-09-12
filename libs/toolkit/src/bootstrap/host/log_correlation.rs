//! Trace-id injection for JSON log records.
//!
//! Logs are not exported over OTLP — they go to stderr and to the rotating
//! files, and a collector picks them up from there. For a backend to link a log
//! line to the span that produced it, the ids have to be *in the record*, at the
//! top level: nested under `"span"` they are invisible to Datadog and most other
//! backends without a bespoke remapper.
//!
//! The stock `fmt::layer().json()` formatter cannot add computed fields, so
//! [`TraceIdJson`] wraps it: the inner formatter produces the record exactly as
//! it does today, and the two ids are spliced in before the closing brace. That
//! keeps the output byte-identical to the current shape apart from the two added
//! keys — which is why this composes with the stock formatter instead of
//! reimplementing it, where it would silently drift from upstream.
//!
//! Enabled by `opentelemetry.tracing.logs_correlation.inject_trace_ids_into_logs`.
//! It is off by default because resolving the context costs a lookup on every
//! event.

use std::fmt;

use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{Format, Json, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// A JSON event formatter that adds top-level `trace_id` and `span_id`.
///
/// Delegates to the stock JSON formatter and splices the ids into the result.
/// Records emitted outside a valid span pass through untouched.
pub struct TraceIdJson<T> {
    inner: Format<Json, T>,
    /// When false this is a transparent wrapper — no context lookup, no
    /// splice. Keeping the type the same either way lets the caller build one
    /// layer type instead of branching at the type level.
    enabled: bool,
}

impl<T> TraceIdJson<T> {
    pub const fn new(inner: Format<Json, T>, enabled: bool) -> Self {
        Self { inner, enabled }
    }
}

impl<S, N, T> FormatEvent<S, N> for TraceIdJson<T>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    N: for<'writer> FormatFields<'writer> + 'static,
    T: FormatTime,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        if !self.enabled {
            return self.inner.format_event(ctx, writer, event);
        }

        let Some(ids) = current_trace_ids() else {
            // No active OTel span (or no tracer installed): nothing to add.
            return self.inner.format_event(ctx, writer, event);
        };

        let mut buf = String::new();
        self.inner.format_event(ctx, Writer::new(&mut buf), event)?;

        match splice_ids(&buf, &ids) {
            Some(line) => writer.write_str(&line),
            // The inner formatter produced something that is not the expected
            // `{...}` object. Emitting it unchanged is strictly better than
            // corrupting it.
            None => writer.write_str(&buf),
        }
    }
}

/// The ids of the currently active span, if there is a valid one.
///
/// Reads OpenTelemetry's own thread-local context. `OpenTelemetrySpanExt::context()`
/// would be the obvious route but it re-enters the subscriber to build the span,
/// which is not possible from inside `on_event` — it silently yields an empty
/// context there. `OpenTelemetryLayer` attaches the context on span entry
/// (`context_activation`, on by default), so by the time an event is formatted
/// the current context already carries the span.
fn current_trace_ids() -> Option<(String, String)> {
    use opentelemetry::trace::TraceContextExt as _;

    let context = opentelemetry::Context::current();
    let span = context.span();
    let span_context = span.span_context();
    if !span_context.is_valid() {
        return None;
    }
    // `Display` for both is lowercase hex (32 and 16 chars), matching the
    // `traceparent` wire form and `toolkit_http::otel::parse_trace_id`.
    Some((
        span_context.trace_id().to_string(),
        span_context.span_id().to_string(),
    ))
}

/// Insert the ids into a rendered JSON object, before its closing brace.
///
/// Returns `None` when `line` is not a `{...}` object, leaving the caller to
/// pass the original through.
fn splice_ids(line: &str, (trace_id, span_id): &(String, String)) -> Option<String> {
    let trailing_newlines = line.len() - line.trim_end_matches('\n').len();
    let body = line.trim_end_matches('\n');

    let head = body.strip_suffix('}')?;
    if !head.starts_with('{') {
        return None;
    }

    // An empty object (`{}`) takes no separating comma.
    let separator = if head.trim_end() == "{" { "" } else { "," };

    let mut out = String::with_capacity(line.len() + 80);
    out.push_str(head);
    out.push_str(separator);
    // Both ids are hex, so they need no JSON escaping.
    out.push_str(r#""trace_id":""#);
    out.push_str(trace_id);
    out.push_str(r#"","span_id":""#);
    out.push_str(span_id);
    out.push_str("\"}");
    for _ in 0..trailing_newlines {
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::splice_ids;

    fn ids() -> (String, String) {
        (
            "4bf92f3577b34da6a3ce929d0e0e4736".to_owned(),
            "00f067aa0ba902b7".to_owned(),
        )
    }

    #[test]
    fn adds_ids_before_the_closing_brace() {
        let out = splice_ids(r#"{"level":"INFO","message":"hi"}"#, &ids()).expect("object");
        assert_eq!(
            out,
            r#"{"level":"INFO","message":"hi","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"00f067aa0ba902b7"}"#
        );
    }

    /// The fmt layer terminates records with a newline; it must stay last so the
    /// line-delimited JSON stream is not broken.
    #[test]
    fn preserves_the_trailing_newline() {
        let out = splice_ids("{\"a\":1}\n", &ids()).expect("object");
        assert!(out.ends_with("}\n"), "got {out:?}");
        assert_eq!(out.matches('\n').count(), 1);
    }

    /// An empty object must not gain a leading comma.
    #[test]
    fn empty_object_gets_no_separator() {
        let out = splice_ids("{}", &ids()).expect("object");
        assert!(out.starts_with(r#"{"trace_id":"#), "got {out:?}");
    }

    /// Anything that is not a JSON object is passed back as unsplicable rather
    /// than corrupted.
    #[test]
    fn non_object_input_is_rejected() {
        assert!(splice_ids("not json", &ids()).is_none());
        assert!(splice_ids("[1,2,3]", &ids()).is_none());
        assert!(splice_ids("", &ids()).is_none());
    }

    // ===== end-to-end through a real subscriber =============================

    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// Collects formatted records so a test can read them back.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        fn contents(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            )
            .unwrap_or_default()
        }
    }

    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `emit` under a subscriber wired like the production JSON sink, and
    /// return what that sink wrote.
    fn capture_with(enabled: bool, emit: impl FnOnce()) -> String {
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("log-correlation-test");

        let buffer = Buffer::default();
        let fmt_layer = tracing_subscriber::fmt::layer()
            .json()
            .event_format(super::TraceIdJson::new(
                tracing_subscriber::fmt::format()
                    .json()
                    .with_ansi(false)
                    .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339()),
                enabled,
            ))
            .with_writer(buffer.clone());

        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .with(fmt_layer);

        tracing::subscriber::with_default(subscriber, emit);

        buffer.contents()
    }

    /// Emit one event inside a span and return what the JSON sink wrote.
    fn capture(enabled: bool, inside_span: bool) -> String {
        capture_with(enabled, || {
            if inside_span {
                tracing::info_span!("unit").in_scope(|| tracing::info!("hello"));
            } else {
                tracing::info!("hello");
            }
        })
    }

    /// An event carrying its own `trace_id`/`span_id` fields must not shadow
    /// the spliced ids. The stock formatter nests event fields under `fields`
    /// (`flatten_event` is off), so the splice owns the top level outright.
    /// This pins that: enabling `flatten_event` would move the event's copies
    /// up and produce duplicate keys, and this test fails if that ever happens.
    #[test]
    fn event_fields_do_not_shadow_the_spliced_ids() {
        let out = capture_with(true, || {
            tracing::info_span!("unit").in_scope(|| {
                tracing::info!(trace_id = "from_event", span_id = "from_event", "hello");
            });
        });
        let value: serde_json::Value =
            serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON: {e}: {out}"));

        assert_eq!(
            value["trace_id"].as_str().unwrap_or_default().len(),
            32,
            "top-level trace_id must be the live span's, not the event's: {out}"
        );
        assert_eq!(
            value["span_id"].as_str().unwrap_or_default().len(),
            16,
            "top-level span_id must be the live span's, not the event's: {out}"
        );
        assert_eq!(
            value["fields"]["trace_id"], "from_event",
            "the event's own field must stay nested under `fields`: {out}"
        );
        assert_eq!(
            value["fields"]["span_id"], "from_event",
            "the event's own field must stay nested under `fields`: {out}"
        );
    }

    /// The real payoff: ids resolved from the live span context, not a fixture.
    #[test]
    fn injects_ids_for_an_event_inside_a_span() {
        let out = capture(true, true);
        let value: serde_json::Value =
            serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON: {e}: {out}"));

        let trace_id = value["trace_id"].as_str().unwrap_or_default();
        let span_id = value["span_id"].as_str().unwrap_or_default();

        assert_eq!(trace_id.len(), 32, "trace_id should be 32 hex chars: {out}");
        assert_eq!(span_id.len(), 16, "span_id should be 16 hex chars: {out}");
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()), "{out}");
        assert_ne!(trace_id, "0".repeat(32), "trace_id must not be the nil id");
    }

    /// Disabled is a transparent passthrough — the stock record, unmodified.
    #[test]
    fn adds_nothing_when_disabled() {
        let out = capture(false, true);
        assert!(!out.contains("trace_id"), "{out}");
        assert!(out.contains("hello"), "record still emitted: {out}");
    }

    /// No active span means no ids, and no panic.
    #[test]
    fn adds_nothing_outside_a_span() {
        let out = capture(true, false);
        assert!(!out.contains("trace_id"), "{out}");
        assert!(out.contains("hello"), "record still emitted: {out}");
    }
}
