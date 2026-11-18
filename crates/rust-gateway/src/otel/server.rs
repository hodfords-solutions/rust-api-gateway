//! Server spans (OTEL-02) and the context they give the rest of a request.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context as TaskContext, Poll};

use axum::body::Body;
use axum::extract::{ConnectInfo, MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use http::{Extensions, Version, header};
use http_body::{Body as HttpBody, Frame, SizeHint};
use opentelemetry::context::FutureExt as _;
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt as _, Tracer as _};
use opentelemetry::{Context, KeyValue};

use super::client::{Lines, normalize_method};

/// What handled a request, which names its server span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Matched {
    /// The proxy (the reference's catch-all route `/{*splat}`).
    Proxy,
    /// A gateway endpoint, by its Express route path.
    Endpoint(&'static str),
    /// An application route, by axum's matched path.
    Route(String),
}

/// Where the handler of a traced request records itself.
#[derive(Clone, Default)]
struct Slot(Arc<Mutex<Option<Matched>>>);

/// The context of a traced request (its server span), for work that leaves the request's
/// task, such as an MCP tool call.
#[cfg(feature = "mcp")]
#[derive(Clone)]
struct RequestContext(Context);

/// Records what handled a request, when the request is traced.
pub(crate) fn record(extensions: &Extensions, matched: Matched) {
    if let Some(slot) = extensions.get::<Slot>() {
        *slot.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(matched);
    }
}

/// Layer of the application's routes: records the matched route of a traced request.
pub(crate) async fn record_matched_path(request: Request, next: Next) -> Response {
    if let Some(path) = request.extensions().get::<MatchedPath>() {
        record(
            request.extensions(),
            Matched::Route(path.as_str().to_string()),
        );
    }
    next.run(request).await
}

/// Runs `future` in the context of the traced request whose extensions these are.
#[cfg(feature = "mcp")]
pub(crate) async fn in_request_context<F: std::future::Future>(
    extensions: &Extensions,
    future: F,
) -> F::Output {
    match extensions.get::<RequestContext>() {
        Some(RequestContext(cx)) => future.with_context(cx.clone()).await,
        None => future.await,
    }
}

/// The outermost layer of the gateway's router: one server span per request while tracing
/// is started (OTEL-02), continuing the incoming trace context (OTEL-04).
pub(crate) async fn server_span(mut request: Request, next: Next) -> Response {
    let Some(active) = super::active() else {
        return next.run(request).await;
    };
    let headers = request.headers();
    let parent = active.propagator.extract_with_context(
        &Context::new(),
        &Lines::collect(headers.iter().map(|(n, v)| (n.as_str(), v.as_bytes()))),
    );
    let method = request.method().as_str().to_string();
    let target = request.uri().to_string();
    let normalized = crate::request::normalize_request_url(&target).unwrap_or(target);
    let (pathname, query) = match normalized.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query)),
        None => (normalized.clone(), None),
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);

    // `@opentelemetry/instrumentation-http`'s attributes of an incoming request.
    let normalized_method = normalize_method(&method);
    let mut attributes = vec![
        KeyValue::new("http.request.method", normalized_method.to_string()),
        KeyValue::new("url.scheme", "http"),
        KeyValue::new("url.path", pathname.clone()),
        KeyValue::new(
            "network.protocol.version",
            if request.version() == Version::HTTP_10 {
                "1.0"
            } else {
                "1.1"
            },
        ),
    ];
    if normalized_method != method {
        attributes.push(KeyValue::new(
            "http.request.method_original",
            method.clone(),
        ));
    }
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        attributes.push(KeyValue::new("url.query", redact_query(query)));
    }
    let header = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some((host, port)) = header(header::HOST).and_then(split_host) {
        attributes.push(KeyValue::new("server.address", host.to_string()));
        if let Some(port) = port {
            attributes.push(KeyValue::new("server.port", i64::from(port)));
        }
    }
    if let Some(user_agent) = header(header::USER_AGENT) {
        attributes.push(KeyValue::new("user_agent.original", user_agent.to_string()));
    }
    let peer_address = peer.map(|p| crate::request::node_remote_address(p.ip()));
    if let Some(peer) = peer {
        attributes.push(KeyValue::new(
            "network.peer.address",
            peer_address.clone().unwrap_or_default(),
        ));
        attributes.push(KeyValue::new("network.peer.port", i64::from(peer.port())));
    }
    let forwarded_for = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if let Some(client) = forwarded_for.or(peer_address) {
        attributes.push(KeyValue::new("client.address", client));
    }

    let span = active
        .tracer
        .span_builder(method.clone())
        .with_kind(SpanKind::Server)
        .with_attributes(attributes)
        .start_with_context(&active.tracer, &parent);
    let cx = parent.with_span(span);
    let slot = Slot::default();
    request.extensions_mut().insert(slot.clone());
    #[cfg(feature = "mcp")]
    request.extensions_mut().insert(RequestContext(cx.clone()));

    let response = next.run(request).with_context(cx.clone()).await;

    let span = cx.span();
    let status = response.status().as_u16();
    span.set_attribute(KeyValue::new(
        "http.response.status_code",
        i64::from(status),
    ));
    if !(100..500).contains(&status) {
        span.set_attribute(KeyValue::new("error.type", status.to_string()));
        span.set_status(Status::error(""));
    }
    let matched = slot.0.lock().unwrap_or_else(PoisonError::into_inner).take();
    match matched {
        // The reference's hook renames the catch-all route's span `<METHOD> <pathname>`.
        Some(Matched::Proxy) => {
            span.set_attribute(KeyValue::new("http.route", "/{*splat}"));
            span.update_name(format!("{method} {pathname}"));
        }
        Some(Matched::Endpoint(route)) => {
            span.set_attribute(KeyValue::new("http.route", route));
            span.update_name(format!("{method} {route}"));
        }
        Some(Matched::Route(route)) => {
            span.update_name(format!("{method} {route}"));
            span.set_attribute(KeyValue::new("http.route", route));
        }
        None => {}
    }
    response.map(|body| Body::new(Traced { inner: body, cx }))
}

/// The query as the attribute carries it, with the values of the parameters the reference's
/// instrumentation redacts replaced by `REDACTED`.
fn redact_query(query: &str) -> String {
    const REDACTED: &[&str] = &[
        "sig",
        "Signature",
        "AWSAccessKeyId",
        "X-Goog-Signature",
        "X-Amz-Signature",
        "X-Amz-Credential",
        "X-Amz-Security-Token",
    ];
    query
        .split('&')
        .map(|pair| {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key);
            if REDACTED.contains(&key) {
                format!("{key}=REDACTED")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// `host[:port]` of a `Host` header; an IPv6 address loses its brackets.
fn split_host(host: &str) -> Option<(&str, Option<u16>)> {
    if host.is_empty() {
        return None;
    }
    if let Some(rest) = host.strip_prefix('[') {
        let (address, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
        return Some((address, port));
    }
    match host.rsplit_once(':') {
        Some((name, port)) => Some((name, port.parse().ok())),
        None => Some((host, None)),
    }
}

/// A response body whose request's server span ends when the body is done with (sent, or
/// dropped when the client goes away), as Node's instrumentation ends it on `close`.
struct Traced {
    inner: Body,
    cx: Context,
}

impl HttpBody for Traced {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for Traced {
    fn drop(&mut self) {
        self.cx.span().end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_redacted_like_the_reference() {
        assert_eq!(redact_query("a=1&sig=x&b"), "a=1&sig=REDACTED&b");
        assert_eq!(
            redact_query("X-Amz-Signature=s"),
            "X-Amz-Signature=REDACTED"
        );
    }

    #[test]
    fn hosts_are_split() {
        assert_eq!(split_host("gw:8080"), Some(("gw", Some(8080))));
        assert_eq!(split_host("gw"), Some(("gw", None)));
        assert_eq!(split_host("[::1]:80"), Some(("::1", Some(80))));
        assert_eq!(split_host(""), None);
    }
}
