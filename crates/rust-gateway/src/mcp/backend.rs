//! The gateway's [`ToolCallBackend`]: rate limiting and the proxy middleware in the MCP order
//! (MCP-15, MCP-16), and the upstream request over a connection pool per Upstream Service
//! (MCP-17, MCP-18, MCP-20).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Request};
use http_body_util::BodyExt as _;
use serde_json::Value;

use super::executor::{
    BoxFuture, ToolCallBackend, ToolCallContext, ToolCallRejection, UpstreamCall, UpstreamError,
    UpstreamResponse,
};
use super::server::peer_addr;
use crate::error::MiddlewareBody;
use crate::gateway::Handlers;
use crate::handler::{GatewayRequest, IdentityContext, MiddlewareError};
use crate::proxy::pool::{RawHead, UpstreamPool};
use crate::rate_limit::{RateLimitError, RateLimiter, RouteKey};
use crate::request::{ProxyRequest, normalize_request_url};
use crate::route::RouteMethod;

/// Runs tool calls through the gateway's own rate limiter, handlers and pools.
pub(crate) struct GatewayBackend {
    pub(crate) limiter: RateLimiter,
    pub(crate) handlers: Handlers,
    pub(crate) exclude_headers: Vec<String>,
    /// One pool per Upstream Service, separate from the forwarding pools as in the reference
    /// (its executor creates its own undici `Pool`s), with the same options (CFG-10, MCP-20).
    pub(crate) pools: HashMap<String, Arc<UpstreamPool>>,
}

impl std::fmt::Debug for GatewayBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayBackend")
            .field("handlers", &self.handlers)
            .field("pools", &self.pools.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl GatewayBackend {
    async fn run_authorize(
        &self,
        context: ToolCallContext<'_>,
    ) -> Result<ProxyRequest, ToolCallRejection> {
        let parts = context.request;
        let route = &context.tool.route;
        let peer = peer_addr(&parts.extensions);
        let target = parts.uri.to_string();
        let url = normalize_request_url(&target).unwrap_or(target);
        let body = Value::Object(context.arguments.clone());

        // MCP-15: the `/mcp` request is rate-limited before any middleware runs.
        let limiter = &self.limiter;
        let mut state = limiter.request_state(peer.map(|p| p.ip()), &parts.headers);
        limiter
            .count_global_ip(&mut state)
            .await
            .map_err(rejection)?;
        // The resolver sees the `/mcp` request with its headers as received (before the
        // `excludeHeaders` are removed) and the tool arguments as its body.
        let mut request = GatewayRequest::new(
            Method::POST,
            parts.uri.clone(),
            url.clone(),
            parts.headers.clone(),
            peer,
        )
        .with_version(parts.version)
        .with_body(body.clone())
        .with_extensions(parts.extensions.clone());
        let identity = if limiter.is_enabled() {
            match &self.handlers.identity_resolver {
                Some(resolver) => resolver
                    .resolve(IdentityContext {
                        request: &request,
                        route,
                        ip: state.client_key(),
                    })
                    .await
                    .map_err(|e| ToolCallRejection::Failed(e.to_string()))?,
                None => Some(state.default_identity()),
            }
        } else {
            None
        };
        limiter
            .check_limits(
                &mut state,
                identity,
                RouteKey::new(&context.tool.service, "POST", &route.router_path),
                &route.rate_limits,
            )
            .await
            .map_err(rejection)?;

        // MCP-16: then the proxy middleware, on the request without the `excludeHeaders`.
        let extensions = request.into_extensions();
        request = GatewayRequest::new(
            Method::POST,
            parts.uri.clone(),
            url,
            context.middleware_headers.clone(),
            peer,
        )
        .with_version(parts.version)
        .with_body(body)
        .with_extensions(extensions);
        let mut proxy_request = ProxyRequest::new();
        for name in &self.exclude_headers {
            proxy_request.remove_header(name);
        }
        for middleware in self.handlers.proxy.iter() {
            match middleware
                .handle(route, &mut request, &mut proxy_request)
                .await
            {
                Ok(true) => {}
                Ok(false) => return Err(ToolCallRejection::Forbidden),
                Err(error) => return Err(ToolCallRejection::Failed(error_message(&error))),
            }
        }
        Ok(proxy_request)
    }

    async fn run_send(
        &self,
        service: &str,
        call: UpstreamCall,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let pool = self.pools.get(service).ok_or_else(|| UpstreamError {
            message: format!("Service unavailable: {service}"),
        })?;
        let request = upstream_request(pool.origin().authority(), &call)?;
        // OTEL-03, OTEL-04: the client span, its headers after the call's (where undici's
        // instrumentation adds them), once the request is known to be valid.
        #[cfg(feature = "otel")]
        let (request, trace) = match traced(pool, &call) {
            Some((call, span)) => (
                upstream_request(pool.origin().authority(), &call)?,
                Some(span),
            ),
            None => (request, None),
        };
        let response = pool.send(request).await.map_err(|e| {
            #[cfg(feature = "otel")]
            if let Some(span) = &trace {
                span.fail(&e, e.trace_type());
            }
            UpstreamError {
                message: e.to_string(),
            }
        })?;
        let status = response.status().as_u16();
        #[cfg(feature = "otel")]
        if let Some(span) = &trace {
            span.response(status);
        }
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|e| UpstreamError {
                message: e.to_string(),
            })?
            .to_bytes();
        Ok(UpstreamResponse {
            status,
            body: body.to_vec(),
        })
    }
}

impl ToolCallBackend for GatewayBackend {
    fn authorize<'a>(
        &'a self,
        context: ToolCallContext<'a>,
    ) -> BoxFuture<'a, Result<ProxyRequest, ToolCallRejection>> {
        Box::pin(self.run_authorize(context))
    }

    fn send<'a>(
        &'a self,
        service: &'a str,
        call: UpstreamCall,
    ) -> BoxFuture<'a, Result<UpstreamResponse, UpstreamError>> {
        Box::pin(self.run_send(service, call))
    }
}

fn rejection(error: RateLimitError) -> ToolCallRejection {
    match error {
        RateLimitError::TooManyRequests(rejection) => ToolCallRejection::TooManyRequests(rejection),
        other => ToolCallRejection::Failed(other.to_string()),
    }
}

/// The message of a middleware error as the reference reads `error.message` (MCP-16): for an
/// HTTP error (Nest's `HttpException`), its body's `message` when that is a string, its body
/// when the body is a string, and otherwise `Http Exception`; for any other error, its
/// message.
fn error_message(error: &MiddlewareError) -> String {
    match error {
        MiddlewareError::Http { body, .. } => match body {
            MiddlewareBody::Text(text) | MiddlewareBody::Json(Value::String(text)) => text.clone(),
            MiddlewareBody::Json(Value::Object(object)) => match object.get("message") {
                Some(Value::String(message)) => message.clone(),
                _ => "Http Exception".to_string(),
            },
            MiddlewareBody::Json(_) => "Http Exception".to_string(),
        },
        MiddlewareError::Other(source) => source.to_string(),
    }
}

/// The request undici writes for a tool call (MCP-17, MCP-18): `host`, then `connection:
/// keep-alive` (`close` for `HEAD`), the call's headers in order and case with duplicates
/// kept, then `content-length` (`0` for `POST`, `PUT` and `PATCH` without a body), and the
/// raw target, which may contain `{x}`.
///
/// The call's headers that undici handles itself follow its rules, so that the head always
/// frames the body sent and a pooled connection stays in step:
///
/// - `host` is written in place of the Upstream Service's authority; a second one refuses the
///   call (`duplicate host header`);
/// - `content-length` is never written as given: it must be digits (`invalid content-length
///   header`), given once (`duplicate content-length header`), and equal to the body's length
///   when there is a body (`Request body length does not match content-length header`);
/// - `connection` must be a list of tokens (`invalid connection header`) and is not written;
///   one of `close` writes `connection: close` and the connection is not reused;
/// - `transfer-encoding`, `keep-alive` and `upgrade` refuse the call (`invalid <name>
///   header`), and so does `expect` (`expect header not supported`).
///
/// Names are compared case-insensitively, and each header is checked in order, so the first
/// one undici would refuse gives the message. A refused call sends nothing.
///
/// hyper writes a header map grouped by name and in lower case, so the exact head travels as
/// the request's [`RawHead`] extension, which the MCP pools' connections write instead of
/// hyper's head; the request hyper sees carries only the framing.
fn upstream_request(
    authority: &str,
    call: &UpstreamCall,
) -> Result<Request<crate::proxy::pool::RequestBody>, UpstreamError> {
    let method = call.method_name();
    let body = call.body.clone().map(Bytes::from);
    let target = &call.path_and_query;
    if target.is_empty()
        || !target.starts_with('/')
        || target.bytes().any(|b| !(0x21..=0x7e).contains(&b))
    {
        return Err(UpstreamError {
            message: "invalid request path".to_string(),
        });
    }

    let mut host: Option<&str> = None;
    let mut declared_length: Option<&str> = None;
    let mut close = call.method == RouteMethod::Head;
    let mut lines = String::new();
    for (name, value) in &call.headers {
        HeaderName::from_bytes(name.as_bytes()).map_err(|_| UpstreamError {
            message: format!("invalid header key {name:?}"),
        })?;
        header_value(name, value)?;
        match name.to_ascii_lowercase().as_str() {
            "host" => {
                if host.replace(value).is_some() {
                    return Err(refused("duplicate host header"));
                }
            }
            "content-length" => {
                if declared_length.is_some() {
                    return Err(refused("duplicate content-length header"));
                }
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(refused("invalid content-length header"));
                }
                declared_length = Some(value);
            }
            lower @ ("transfer-encoding" | "keep-alive" | "upgrade") => {
                return Err(refused(&format!("invalid {lower} header")));
            }
            "connection" => {
                for token in value.to_ascii_lowercase().split(',') {
                    let token = token.trim();
                    if token.is_empty() || !token.bytes().all(is_token_byte) {
                        return Err(refused("invalid connection header"));
                    }
                    close |= token == "close";
                }
            }
            "expect" => return Err(refused("expect header not supported")),
            _ => {
                let _ = write!(lines, "{name}: {value}\r\n");
            }
        }
    }
    let body_length = body.as_ref().map_or(0, Bytes::len);
    if let Some(declared) = declared_length
        && body_length > 0
        && declared.trim_start_matches('0') != body_length.to_string()
    {
        return Err(refused(
            "Request body length does not match content-length header",
        ));
    }
    let length = match (&body, call.method) {
        (Some(_), _) => Some(body_length),
        (None, RouteMethod::Post | RouteMethod::Put | RouteMethod::Patch) => Some(0),
        (None, _) => None,
    };
    let connection = if close { "close" } else { "keep-alive" };

    let host = host.unwrap_or(authority);
    let mut head =
        format!("{method} {target} HTTP/1.1\r\nhost: {host}\r\nconnection: {connection}\r\n");
    head.push_str(&lines);
    let mut headers = HeaderMap::with_capacity(3);
    headers.append(header::HOST, header_value("host", authority)?);
    headers.append(header::CONNECTION, HeaderValue::from_static(connection));
    if let Some(length) = length {
        headers.append(header::CONTENT_LENGTH, HeaderValue::from(length));
        let _ = write!(head, "content-length: {length}\r\n");
    }
    head.push_str("\r\n");

    let method = Method::from_bytes(method.as_bytes()).map_err(|_| UpstreamError {
        message: format!("invalid method {method}"),
    })?;
    let mut request = Request::new(crate::proxy::pool::RequestBody::full(
        body.unwrap_or_default(),
    ));
    *request.method_mut() = method;
    // The target hyper sees is a placeholder: the raw head carries the real one.
    *request.uri_mut() = http::Uri::from_static("/");
    *request.headers_mut() = headers;
    request.extensions_mut().insert(RawHead::new(
        head,
        u64::try_from(body_length).unwrap_or(u64::MAX),
    ));
    Ok(request)
}

fn refused(message: &str) -> UpstreamError {
    UpstreamError {
        message: message.to_string(),
    }
}

/// An RFC 9110 `tchar`.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// The client span of a tool call (OTEL-03) when tracing is started, and the call with the
/// span's `traceparent` (and `tracestate`) in place of any it had, after its other headers.
#[cfg(feature = "otel")]
fn traced(
    pool: &UpstreamPool,
    call: &UpstreamCall,
) -> Option<(UpstreamCall, crate::otel::ClientSpan)> {
    if !crate::otel::is_started() {
        return None;
    }
    let user_agent = call
        .headers
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        .map(|(_, value)| value.as_str());
    let origin = pool.origin();
    let url = format!(
        "{}://{}{}",
        origin.scheme(),
        origin.authority(),
        call.path_and_query
    );
    let span = crate::otel::ClientSpan::start(&call.method_name(), &url, user_agent)?;
    let mut call = call.clone();
    call.headers
        .retain(|(name, _)| !crate::otel::is_trace_header(name));
    call.headers.extend(
        span.trace_headers()
            .into_iter()
            .map(|(name, value)| (name.to_string(), value)),
    );
    Some((call, span))
}

fn header_value(name: &str, value: &str) -> Result<HeaderValue, UpstreamError> {
    HeaderValue::from_str(value).map_err(|_| UpstreamError {
        message: format!("invalid {name} header"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: RouteMethod, headers: &[(&str, &str)], body: Option<&str>) -> UpstreamCall {
        UpstreamCall {
            method,
            path_and_query: "/v1/users/{id}/?a=1".to_string(),
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: body.map(str::to_string),
        }
    }

    fn written_head(request: &Request<crate::proxy::pool::RequestBody>) -> String {
        let head = request.extensions().get::<RawHead>().expect("a raw head");
        String::from_utf8(head.head().to_vec()).unwrap()
    }

    // MCP-18: undici's head, with names in their case, duplicates apart, and the framing.
    #[test]
    fn mcp_18_head_is_written_like_undici() {
        let request = upstream_request(
            "127.0.0.1:9",
            &call(
                RouteMethod::Get,
                &[
                    ("content-type", "text/x"),
                    ("7", "seven"),
                    ("Content-Type", "text/plain"),
                    ("X-Up", "Up"),
                ],
                None,
            ),
        )
        .unwrap();
        assert_eq!(
            written_head(&request),
            "GET /v1/users/{id}/?a=1 HTTP/1.1\r\nhost: 127.0.0.1:9\r\nconnection: keep-alive\r\n\
             content-type: text/x\r\n7: seven\r\nContent-Type: text/plain\r\nX-Up: Up\r\n\r\n"
        );
        let head = upstream_request("h", &call(RouteMethod::Head, &[], None)).unwrap();
        assert!(written_head(&head).contains("connection: close\r\n"));
        let post = upstream_request("h", &call(RouteMethod::Post, &[], None)).unwrap();
        assert!(written_head(&post).ends_with("content-length: 0\r\n\r\n"));
        assert_eq!(post.headers()[header::CONTENT_LENGTH], "0");
        let put = upstream_request("h", &call(RouteMethod::Put, &[], Some("{\"a\":1}"))).unwrap();
        assert!(written_head(&put).ends_with("content-length: 7\r\n\r\n"));
        let delete = upstream_request("h", &call(RouteMethod::Delete, &[], None)).unwrap();
        assert!(!written_head(&delete).contains("content-length"));
    }

    // MCP-17: what undici refuses is an error result, not a request.
    #[test]
    fn mcp_17_invalid_targets_and_values_are_refused() {
        let mut spaced = call(RouteMethod::Get, &[], None);
        spaced.path_and_query = "/a b/".to_string();
        assert_eq!(
            upstream_request("h", &spaced)
                .err()
                .map(|e| e.message)
                .as_deref(),
            Some("invalid request path")
        );
        let newline = call(RouteMethod::Get, &[("x-a", "a\r\nb")], None);
        assert_eq!(
            upstream_request("h", &newline)
                .err()
                .map(|e| e.message)
                .as_deref(),
            Some("invalid x-a header")
        );
    }

    // MCP-16: the message of a middleware error.
    #[test]
    fn mcp_16_error_messages_follow_nest() {
        use http::StatusCode;
        let cases = [
            (
                MiddlewareError::json(
                    StatusCode::UNAUTHORIZED,
                    serde_json::json!({"message": "m"}),
                ),
                "m",
            ),
            (
                MiddlewareError::json(StatusCode::UNAUTHORIZED, serde_json::json!({"error": "e"})),
                "Http Exception",
            ),
            (
                MiddlewareError::json(
                    StatusCode::UNAUTHORIZED,
                    serde_json::json!({"message": ["a"]}),
                ),
                "Http Exception",
            ),
            (
                MiddlewareError::text(StatusCode::UNAUTHORIZED, "plain"),
                "plain",
            ),
            (
                MiddlewareError::json(StatusCode::UNAUTHORIZED, serde_json::json!(5)),
                "Http Exception",
            ),
            (MiddlewareError::other("crash"), "crash"),
        ];
        for (error, expected) in cases {
            assert_eq!(error_message(&error), expected);
        }
    }
}
