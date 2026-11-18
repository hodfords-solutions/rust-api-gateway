//! OpenTelemetry tracing (Parity Checklist area 09), behind the cargo feature `otel`.
//!
//! The Reference Gateway ships a separate package whose one function, `tracing(option)`,
//! starts the Node SDK with auto-instrumentation and an OTLP/HTTP protobuf exporter. Rust has
//! no auto-instrumentation, so this module starts an exporter the same way ([`start`],
//! OTEL-01) and the gateway creates the spans itself, at its edges:
//!
//! - **Server spans** (OTEL-02): one per HTTP request the router handles (served by
//!   [`Gateway::serve`](crate::Gateway::serve) or by any server through
//!   [`Gateway::router`](crate::Gateway::router)), kind `SERVER`, named as the reference's
//!   instrumentation names them once its renaming hook has run:
//!
//!   | Request | Span name | `http.route` |
//!   |---|---|---|
//!   | proxied (Routed, Static and Bypass Requests, and proxy errors) | `<METHOD> <pathname>`, the request's path after URL normalisation (REQ-02), without the query | `/{*splat}` |
//!   | `GET /documents`, `GET /document-json` | `GET /documents`, `GET /document-json` | the same path |
//!   | `POST /mcp` | `POST /mcp` | `/mcp` |
//!   | an application route | `<METHOD> <route>`, axum's matched path (`/users/{id}`) | the matched path |
//!   | answered before routing (Express's 400 for a malformed `%`, the asterisk-form target) | `<METHOD>` | none |
//!
//!   A WebSocket upgrade gets no server span: Node emits `upgrade`, not `request`, for it, so
//!   the reference's HTTP instrumentation never starts one. Neither does a request the HTTP
//!   parser rejects.
//! - **Client spans** (OTEL-03): one per request sent to an Upstream Service, kind `CLIENT`,
//!   named `<METHOD>` as undici's and Node's HTTP instrumentations name them: forwarded
//!   requests and MCP tool calls (children of the server span), and the upstream handshake of
//!   a WebSocket upgrade (a child of the client's trace context, if any). Error status and an
//!   `exception` event when the upstream cannot be reached.
//! - **Propagation** (OTEL-04): W3C Trace Context. An incoming `traceparent` (and
//!   `tracestate`) is continued; each upstream request carries the client span's
//!   `traceparent` (and `tracestate` when not empty) in place of the client's.
//!
//! Redis calls are not traced (Q22). Until [`start`] (or [`start_with_provider`]) is called,
//! and after its [`TracingGuard`] is dropped, the gateway creates no span and adds no header
//! (OTEL-05); the check is one atomic load per request.

mod client;
mod server;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};

pub use opentelemetry;
pub use opentelemetry_sdk;

pub(crate) use client::{ClientSpan, io_error_type, is_trace_header};
#[cfg(feature = "mcp")]
pub(crate) use server::in_request_context;
pub(crate) use server::{Matched, record, record_matched_path, server_span};

/// The instrumentation scope of the gateway's spans.
const SCOPE: &str = "rust-gateway";

/// The options of [`start`]: the reference's `OpenTelemetryOptions` without its unused
/// `minDuration`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTelemetryOptions {
    /// The OTLP/HTTP base URL of the collector, for example `http://localhost:4318`. Spans
    /// are posted to `<otlp_url>/v1/traces` (the reference's `otlpUrl`).
    pub otlp_url: String,
    /// The `service.name` resource attribute (the reference's `serviceName`).
    pub service_name: String,
}

impl OpenTelemetryOptions {
    /// Options exporting to `otlp_url` as `service_name`.
    pub fn new(otlp_url: impl Into<String>, service_name: impl Into<String>) -> Self {
        Self {
            otlp_url: otlp_url.into(),
            service_name: service_name.into(),
        }
    }

    /// Where spans are posted: `<otlp_url>/v1/traces`, as the reference builds it.
    pub fn traces_endpoint(&self) -> String {
        format!("{}/v1/traces", self.otlp_url)
    }
}

/// Why [`start`] could not start tracing.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The OTLP exporter could not be built (an invalid URL, for example).
    #[error("cannot build the OTLP exporter: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
    /// The exporter's HTTP client could not be built.
    #[error("cannot build the OTLP exporter's HTTP client: {0}")]
    HttpClient(String),
    /// The thread that builds the exporter failed.
    #[error("cannot build the OTLP exporter: its thread failed")]
    Thread,
}

/// Starts tracing (OTEL-01): the reference's `tracing(option)`.
///
/// Spans are batched and exported over OTLP/HTTP protobuf to
/// [`traces_endpoint`](OpenTelemetryOptions::traces_endpoint), from a background thread,
/// with `service.name` (and the SDK's default resource attributes, which read
/// `OTEL_RESOURCE_ATTRIBUTES`) on the resource. The exporter's other settings follow the
/// standard `OTEL_EXPORTER_OTLP_*` variables (headers, timeout, compression), as the Node
/// exporter's do. An `https` collector is verified against Mozilla's root certificates,
/// built in, as Node uses its bundled store: the host's trust store is never read, so a host
/// without CA certificates can still start tracing (an application needing other roots
/// builds its own exporter and passes it to [`start_with_provider`]). The provider and a W3C Trace Context propagator are also installed as
/// OpenTelemetry's globals, as the Node SDK does, so application code can add its own spans
/// under the gateway's.
///
/// Call it before the gateway serves requests; it may be called inside or outside a Tokio
/// runtime. Keep the returned guard for as long as tracing should run: dropping it (or
/// [`TracingGuard::shutdown`]) stops the gateway's instrumentation, then flushes and shuts
/// the exporter down, blocking until the last batch is sent or times out.
///
/// Starting again replaces the instrumentation of an earlier start; the earlier guard then
/// only shuts its own exporter down.
pub fn start(options: &OpenTelemetryOptions) -> Result<TracingGuard, StartError> {
    let endpoint = options.traces_endpoint();
    let service_name = options.service_name.clone();
    // The blocking HTTP client runs its own runtime; build it away from any async context.
    let provider = std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .tls_backend_preconfigured(crate::tls::default_client_config())
            .timeout(export_timeout())
            .build()
            .map_err(|error| StartError::HttpClient(error.to_string()))?;
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(client)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint)
            .build()?;
        Ok::<_, StartError>(
            SdkTracerProvider::builder()
                .with_resource(Resource::builder().with_service_name(service_name).build())
                .with_batch_exporter(exporter)
                .build(),
        )
    })
    .join()
    .map_err(|_| StartError::Thread)??;
    Ok(start_with_provider(provider))
}

/// The exporter's request timeout, read as the OTLP exporter reads it:
/// `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT`, else `OTEL_EXPORTER_OTLP_TIMEOUT` (milliseconds), else
/// 10 seconds.
fn export_timeout() -> std::time::Duration {
    [
        "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
    ]
    .iter()
    .find_map(|name| std::env::var(name).ok()?.parse::<u64>().ok())
    .map_or(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis,
    )
}

/// Starts tracing with a provider configured by the application (its own exporter, sampler
/// or resource), as [`start`] does with the OTLP one.
pub fn start_with_provider(provider: SdkTracerProvider) -> TracingGuard {
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    opentelemetry::global::set_tracer_provider(provider.clone());
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let active = Arc::new(Active {
        tracer: provider.tracer(SCOPE),
        propagator: TraceContextPropagator::new(),
    });
    {
        let mut slot = ACTIVE.write().unwrap_or_else(PoisonError::into_inner);
        *slot = Some((generation, active));
        STARTED.store(true, Ordering::SeqCst);
    }
    TracingGuard {
        provider: Some(provider),
        generation,
    }
}

/// Whether the gateway currently creates spans: [`start`] was called and its guard is alive.
pub fn is_started() -> bool {
    STARTED.load(Ordering::Acquire)
}

/// Keeps tracing running; see [`start`].
#[derive(Debug)]
#[must_use = "tracing stops when the guard is dropped"]
pub struct TracingGuard {
    provider: Option<SdkTracerProvider>,
    generation: u64,
}

impl TracingGuard {
    /// The tracer provider, for application spans or a manual
    /// [`force_flush`](SdkTracerProvider::force_flush).
    pub fn provider(&self) -> Option<&SdkTracerProvider> {
        self.provider.as_ref()
    }

    /// Stops the gateway's instrumentation, then flushes and shuts the exporter down,
    /// returning the exporter's error, if any. Blocks until done.
    pub fn shutdown(mut self) -> OTelSdkResult {
        self.stop()
    }

    fn stop(&mut self) -> OTelSdkResult {
        {
            let mut slot = ACTIVE.write().unwrap_or_else(PoisonError::into_inner);
            if slot
                .as_ref()
                .is_some_and(|(generation, _)| *generation == self.generation)
            {
                STARTED.store(false, Ordering::SeqCst);
                *slot = None;
            }
        }
        match self.provider.take() {
            Some(provider) => provider.shutdown(),
            None => Ok(()),
        }
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "shutting the OpenTelemetry exporter down failed");
        }
    }
}

/// What the instrumentation uses while tracing is started.
pub(crate) struct Active {
    tracer: SdkTracer,
    propagator: TraceContextPropagator,
}

static STARTED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static ACTIVE: RwLock<Option<(u64, Arc<Active>)>> = RwLock::new(None);

/// The instrumentation, when tracing is started (OTEL-05: one atomic load otherwise).
pub(crate) fn active() -> Option<Arc<Active>> {
    if !STARTED.load(Ordering::Acquire) {
        return None;
    }
    ACTIVE
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, active)| Arc::clone(active))
}
