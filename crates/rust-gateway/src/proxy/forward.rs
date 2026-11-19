//! Forwarding a request upstream and its response back (area 04).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use bytes::Bytes;
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Request, Response, Uri};

use crate::error::{ErrorContext, GatewayError};
use crate::gateway::Shared;
use crate::rate_limit::{RateLimitState, RateLimiter, RouteKey};
use crate::request::{
    Framing, RequestClass, node_remote_address, response_headers, upstream_framing,
    upstream_request_headers, upstream_target,
};
use crate::route::Route;
use crate::server::{ResponseKind, mark};

use super::body::{ClientResponseBody, take_arrived};
use super::pool::UpstreamPool;

/// What the pipeline decided to send upstream.
pub(crate) struct Outgoing<'a> {
    pub(crate) method: Method,
    pub(crate) prefix: &'a str,
    pub(crate) url: &'a str,
    pub(crate) class: RequestClass,
    pub(crate) client_headers: &'a [(HeaderName, HeaderValue)],
    pub(crate) peer: Option<SocketAddr>,
    pub(crate) middleware_headers: Vec<(String, String)>,
}

/// Counting a status-scoped Route Bucket once the response has been sent (RL-16).
pub(crate) struct AfterResponse {
    pub(crate) limiter: RateLimiter,
    pub(crate) state: RateLimitState,
    pub(crate) service: String,
    pub(crate) method: String,
    pub(crate) route: Arc<Route>,
}

impl AfterResponse {
    pub(crate) fn count(self, status: u16) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let key = RouteKey::new(&self.service, &self.method, &self.route.router_path);
            if let Err(error) = self
                .limiter
                .count_after_response(&self.state, key, &self.route.rate_limits, status)
                .await
            {
                tracing::error!(%error, "counting a status-scoped Route limit failed");
            }
        });
    }
}

/// Sends the request upstream and turns the upstream response into the client's response.
pub(crate) async fn forward(
    shared: &Shared,
    pool: &Arc<UpstreamPool>,
    outgoing: Outgoing<'_>,
    body: Body,
    after_response: Option<AfterResponse>,
) -> Result<Response<Body>, GatewayError> {
    let target = upstream_target(outgoing.prefix, outgoing.url, outgoing.class)?;
    let peer = outgoing
        .peer
        .map(|p| node_remote_address(p.ip()))
        .unwrap_or_default();
    let list = upstream_request_headers(
        outgoing.client_headers,
        &peer,
        &outgoing.method,
        &outgoing.middleware_headers,
    )?;
    let declared = list
        .iter()
        .find(|(name, _)| name == header::CONTENT_LENGTH)
        .and_then(|(_, value)| value.to_str().ok()?.parse::<u64>().ok());
    let (arrived, upstream_body) = take_arrived(body, declared)
        .await
        .map_err(|_| GatewayError::UpstreamUnavailable)?;
    let framing = upstream_framing(&outgoing.method, declared, arrived)?;

    // undici writes `host` first, then `connection: keep-alive`, then the list, and the body
    // framing last (FWD-06, FWD-10, FWD-12).
    let mut headers = HeaderMap::with_capacity(list.len() + 2);
    match list.iter().find(|(name, _)| name == header::HOST) {
        Some((_, host)) => headers.append(header::HOST, host.clone()),
        None => headers.append(
            header::HOST,
            HeaderValue::from_str(pool.origin().authority())
                .map_err(|_| GatewayError::UpstreamUnavailable)?,
        ),
    };
    headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    // OTEL-03, OTEL-04: while tracing is started, the client span's `traceparent` (and
    // `tracestate`) replace the client's, at the end of the list, where undici's
    // instrumentation adds them.
    #[cfg(feature = "otel")]
    let trace = client_span(shared, &outgoing, &target, &list);
    #[cfg(feature = "otel")]
    let replaced =
        |name: &HeaderName| trace.is_some() && crate::otel::is_trace_header(name.as_str());
    #[cfg(not(feature = "otel"))]
    let replaced = |_: &HeaderName| false;
    for (name, value) in list {
        if name != header::HOST && name != header::CONTENT_LENGTH && !replaced(&name) {
            headers.append(name, value);
        }
    }
    #[cfg(feature = "otel")]
    if let Some(span) = &trace {
        for (name, value) in span.trace_headers() {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.append(name, value);
            }
        }
    }
    match framing {
        Framing::ContentLength(length) => {
            headers.append(header::CONTENT_LENGTH, HeaderValue::from(length));
        }
        Framing::Chunked => {
            headers.append(
                header::TRANSFER_ENCODING,
                HeaderValue::from_static("chunked"),
            );
        }
        Framing::None => {}
    }
    let uri: Uri = target
        .parse()
        .map_err(|_| GatewayError::UpstreamUnavailable)?;
    let mut request = Request::new(upstream_body);
    *request.method_mut() = outgoing.method;
    *request.uri_mut() = uri;
    *request.headers_mut() = headers;

    let response = pool.send(request).await.map_err(|error| {
        tracing::warn!(%error, "upstream request failed");
        #[cfg(feature = "otel")]
        if let Some(span) = &trace {
            span.fail(&error, error.trace_type());
        }
        GatewayError::UpstreamUnavailable
    })?;
    let (mut parts, upstream) = response.into_parts();

    // FWD-15: the upstream's headers in undici's grouping, X-Powered-By first; Express's own
    // value only when it is turned on (Deviation D11).
    let upstream_powered = parts.headers.contains_key("x-powered-by");
    let mut out = HeaderMap::with_capacity(parts.headers.len() + 1);
    for (name, value) in response_headers(parts.headers.iter()) {
        if !shared.x_powered_by && !upstream_powered && name == "x-powered-by" {
            continue;
        }
        out.append(name, value);
    }
    parts.headers = out;
    // The upstream's reason phrase is not passed on (FWD-14).
    parts.extensions.remove::<hyper::ext::ReasonPhrase>();
    mark(&mut parts.extensions, ResponseKind::Proxied);

    let status = parts.status.as_u16();
    let mut client_body = ClientResponseBody::new(upstream);
    if let Some(after) = after_response {
        client_body = client_body.on_end(move || after.count(status));
    }
    // The client span ends with the upstream response's body, as undici's does.
    #[cfg(feature = "otel")]
    if let Some(span) = trace {
        span.response(status);
        client_body = client_body.on_end(move || drop(span));
    }
    Ok(Response::from_parts(parts, Body::new(client_body)))
}

/// The client span of a forwarded request (OTEL-03), when tracing is started.
#[cfg(feature = "otel")]
fn client_span(
    shared: &Shared,
    outgoing: &Outgoing<'_>,
    target: &str,
    list: &[(HeaderName, HeaderValue)],
) -> Option<crate::otel::ClientSpan> {
    if !crate::otel::is_started() {
        return None;
    }
    let host = &shared
        .options
        .api_services
        .iter()
        .find(|service| service.prefix == outgoing.prefix)?
        .host;
    let user_agent = list
        .iter()
        .rev()
        .find(|(name, _)| name == header::USER_AGENT)
        .and_then(|(_, value)| value.to_str().ok());
    crate::otel::ClientSpan::start(
        outgoing.method.as_str(),
        &format!("{}{target}", host.trim_end_matches('/')),
        user_agent,
    )
}

/// The client's response for a gateway error: the renderer's status, headers and body (REQ-23,
/// REQ-25), with the headers Express adds to a JSON response (`Content-Length` and a weak
/// `ETag`). The proxy's own 503 (FWD-22) is written like the reference writes it, without a
/// length, so it is sent chunked.
pub(crate) fn render_error(shared: &Shared, error: &GatewayError) -> Response<Body> {
    render_error_with(shared, error, &ErrorContext::new())
}

/// [`render_error`], telling the renderer `context` (RL-25).
pub(crate) fn render_error_with(
    shared: &Shared,
    error: &GatewayError,
    context: &ErrorContext,
) -> Response<Body> {
    let rendered = shared.renderer.render_with_context(error, context);
    let (mut parts, body) = rendered.into_parts();
    if !matches!(error, GatewayError::UpstreamUnavailable) {
        if !parts.headers.contains_key(header::CONTENT_LENGTH) {
            parts
                .headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        }
        if !parts.headers.contains_key(header::ETAG)
            && let Ok(etag) = HeaderValue::from_str(&weak_etag(&body))
        {
            parts.headers.insert(header::ETAG, etag);
        }
        mark(&mut parts.extensions, ResponseKind::Express);
    } else {
        mark(&mut parts.extensions, ResponseKind::Streamed);
    }
    Response::from_parts(parts, Body::from(Bytes::from(body)))
}

/// Express's default ETag for a response body (the `etag` package, weak): the length in hex
/// and the first 27 characters of the base64 SHA-1.
pub(crate) fn weak_etag(body: &[u8]) -> String {
    let digest = sha1_smol::Sha1::from(body).digest().bytes();
    let encoded = base64(&digest);
    let hash: String = encoded.chars().take(27).collect();
    format!("W/\"{:x}-{hash}\"", body.len())
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(TABLE[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etags_match_express() {
        // `etag('{"message":"Forbidden","statusCode":403}', {weak: true})` in Node.
        assert_eq!(
            weak_etag(br#"{"message":"Forbidden","statusCode":403}"#),
            "W/\"28-/A0r2gBi62UEXdaQT52VOJ1+hp4\""
        );
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
