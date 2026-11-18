//! Optional OpenTelemetry tracing (Parity Checklist area 09), configured from the
//! environment after the reference package's `OpenTelemetryOptions`:
//!
//! - `OTLP_URL` (`otlpUrl`): the collector's OTLP/HTTP base URL, for example
//!   `http://localhost:4318`. Spans go to `<OTLP_URL>/v1/traces`. Unset or empty: no tracing.
//! - `SERVICE_NAME` (`serviceName`): the `service.name` of the spans. Default
//!   `rust-gateway-server`.

use rust_gateway::otel::{self, OpenTelemetryOptions, TracingGuard};

/// Starts tracing when `OTLP_URL` is set; the guard flushes the exporter when dropped.
pub(crate) fn start() -> Result<Option<TracingGuard>, String> {
    let variable = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let Some(otlp_url) = variable("OTLP_URL") else {
        return Ok(None);
    };
    let service_name =
        variable("SERVICE_NAME").unwrap_or_else(|| "rust-gateway-server".to_string());
    let options = OpenTelemetryOptions::new(otlp_url, service_name);
    let guard = otel::start(&options).map_err(|e| format!("starting tracing: {e}"))?;
    tracing::info!(
        endpoint = %options.traces_endpoint(),
        service = %options.service_name,
        "OpenTelemetry tracing started"
    );
    Ok(Some(guard))
}
