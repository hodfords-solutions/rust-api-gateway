//! Client spans (OTEL-03) and the trace headers sent upstream (OTEL-04).

use std::sync::Arc;

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator as _};
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};

use super::Active;

/// One upstream request's span. It ends when dropped.
pub(crate) struct ClientSpan {
    cx: Context,
    active: Arc<Active>,
}

/// The header names the gateway replaces on an upstream request while tracing is started.
pub(crate) fn is_trace_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("traceparent") || name.eq_ignore_ascii_case("tracestate")
}

impl ClientSpan {
    /// A span for a request to `url` (absolute), a child of the current context: the server
    /// span of the request being handled. `None` while tracing is not started.
    pub(crate) fn start(method: &str, url: &str, user_agent: Option<&str>) -> Option<Self> {
        let active = super::active()?;
        let parent = Context::current();
        Some(Self::with_parent(active, &parent, method, url, user_agent))
    }

    /// A span for a request to `url`, a child of the trace context in `headers` (a new trace
    /// when they carry none). `None` while tracing is not started.
    pub(crate) fn start_from_headers<'a>(
        headers: impl Iterator<Item = (&'a str, &'a [u8])>,
        method: &str,
        url: &str,
        user_agent: Option<&str>,
    ) -> Option<Self> {
        let active = super::active()?;
        let parent = active
            .propagator
            .extract_with_context(&Context::new(), &Lines::collect(headers));
        Some(Self::with_parent(active, &parent, method, url, user_agent))
    }

    fn with_parent(
        active: Arc<Active>,
        parent: &Context,
        method: &str,
        url: &str,
        user_agent: Option<&str>,
    ) -> Self {
        let normalized = normalize_method(method);
        let name = if normalized == "_OTHER" {
            "HTTP".to_string()
        } else {
            normalized.to_string()
        };
        // undici's attributes (`@opentelemetry/instrumentation-undici`).
        let mut attributes = vec![
            KeyValue::new("http.request.method", normalized.to_string()),
            KeyValue::new("http.request.method_original", method.to_string()),
            KeyValue::new("url.full", url.to_string()),
        ];
        if let Ok(parsed) = url::Url::parse(url) {
            attributes.push(KeyValue::new("url.path", parsed.path().to_string()));
            attributes.push(KeyValue::new(
                "url.query",
                parsed.query().map(|q| format!("?{q}")).unwrap_or_default(),
            ));
            attributes.push(KeyValue::new("url.scheme", parsed.scheme().to_string()));
            if let Some(host) = parsed.host_str() {
                let host = host.trim_start_matches('[').trim_end_matches(']');
                attributes.push(KeyValue::new("server.address", host.to_string()));
            }
            if let Some(port) = parsed.port_or_known_default() {
                attributes.push(KeyValue::new("server.port", i64::from(port)));
            }
        }
        if let Some(user_agent) = user_agent {
            attributes.push(KeyValue::new("user_agent.original", user_agent.to_string()));
        }
        let span = active
            .tracer
            .span_builder(name)
            .with_kind(SpanKind::Client)
            .with_attributes(attributes)
            .start_with_context(&active.tracer, parent);
        Self {
            cx: parent.with_span(span),
            active,
        }
    }

    /// The headers that carry this span's context upstream: `traceparent`, then
    /// `tracestate` when the trace state is not empty.
    pub(crate) fn trace_headers(&self) -> Vec<(&'static str, String)> {
        let mut injected = Injected::default();
        self.active
            .propagator
            .inject_context(&self.cx, &mut injected);
        let mut out = Vec::with_capacity(2);
        if let Some(value) = injected.traceparent {
            out.push(("traceparent", value));
        }
        if let Some(value) = injected.tracestate.filter(|v| !v.is_empty()) {
            out.push(("tracestate", value));
        }
        out
    }

    /// Records the upstream's status: an error from 400 on, as undici's instrumentation does.
    pub(crate) fn response(&self, status: u16) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new(
            "http.response.status_code",
            i64::from(status),
        ));
        if status >= 400 {
            span.set_attribute(KeyValue::new("error.type", status.to_string()));
            span.set_status(Status::error(""));
        }
    }

    /// Records a request that got no response: `error.type`, an `exception` event and an
    /// error status with the error's message.
    pub(crate) fn fail(&self, error: &(dyn std::error::Error + 'static), error_type: &str) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new("error.type", error_type.to_string()));
        span.record_error(error);
        span.set_status(Status::error(error.to_string()));
    }
}

impl Drop for ClientSpan {
    fn drop(&mut self) {
        self.cx.span().end();
    }
}

/// The `error.type` of an I/O failure: the code Node gives it.
pub(crate) fn io_error_type(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::UnexpectedEof => "ECONNRESET",
        std::io::ErrorKind::TimedOut => "ETIMEDOUT",
        std::io::ErrorKind::InvalidData => "HPE_INVALID",
        _ => "Error",
    }
}

/// `http.request.method` as the semantic conventions normalise it.
pub(super) fn normalize_method(method: &str) -> &str {
    match method {
        "GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "CONNECT" | "OPTIONS" | "TRACE" | "PATCH"
        | "QUERY" => method,
        _ => "_OTHER",
    }
}

/// Request header lines, read by the propagator.
pub(super) struct Lines(Vec<(String, String)>);

impl Lines {
    pub(super) fn collect<'a>(headers: impl Iterator<Item = (&'a str, &'a [u8])>) -> Self {
        Self(
            headers
                .filter(|(name, _)| is_trace_header(name))
                .filter_map(|(name, value)| {
                    Some((
                        name.to_ascii_lowercase(),
                        std::str::from_utf8(value).ok()?.to_string(),
                    ))
                })
                .collect(),
        )
    }
}

impl Extractor for Lines {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.iter().map(|(name, _)| name.as_str()).collect()
    }
}

#[derive(Default)]
struct Injected {
    traceparent: Option<String>,
    tracestate: Option<String>,
}

impl Injector for Injected {
    fn set(&mut self, key: &str, value: String) {
        match key {
            "traceparent" => self.traceparent = Some(value),
            "tracestate" => self.tracestate = Some(value),
            _ => {}
        }
    }
}
