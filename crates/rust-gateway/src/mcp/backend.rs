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
use crate::proxy::pool::{RAW_HEAD_HEADER, UpstreamPool};
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
        let response = pool.send(request).await.map_err(|e| UpstreamError {
            message: e.to_string(),
        })?;
        let status = response.status().as_u16();
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
/// hyper writes a header map grouped by name and in lower case, so the exact head travels
/// in the [`RAW_HEAD_HEADER`] header, which the MCP pools' connections write instead of
/// hyper's head; the header map hyper sees has the same framing.
fn upstream_request(
    authority: &str,
    call: &UpstreamCall,
) -> Result<Request<crate::proxy::pool::RequestBody>, UpstreamError> {
    let method = call.method_name();
    let body = call.body.clone().map(Bytes::from);
    let length = match (&body, call.method) {
        (Some(body), _) => Some(body.len()),
        (None, RouteMethod::Post | RouteMethod::Put | RouteMethod::Patch) => Some(0),
        (None, _) => None,
    };
    let connection = if call.method == RouteMethod::Head {
        "close"
    } else {
        "keep-alive"
    };
    let target = &call.path_and_query;
    if target.is_empty()
        || !target.starts_with('/')
        || target.bytes().any(|b| !(0x21..=0x7e).contains(&b))
    {
        return Err(UpstreamError {
            message: "invalid request path".to_string(),
        });
    }

    let mut head =
        format!("{method} {target} HTTP/1.1\r\nhost: {authority}\r\nconnection: {connection}\r\n");
    let mut headers = HeaderMap::with_capacity(call.headers.len() + 4);
    headers.append(header::HOST, header_value("host", authority)?);
    headers.append(header::CONNECTION, HeaderValue::from_static(connection));
    for (name, value) in &call.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| UpstreamError {
            message: format!("invalid header key {name:?}"),
        })?;
        headers.append(header_name, header_value(name, value)?);
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if let Some(length) = length {
        headers.append(header::CONTENT_LENGTH, HeaderValue::from(length));
        let _ = write!(head, "content-length: {length}\r\n");
    }
    head.push_str("\r\n");
    headers.append(
        HeaderName::from_static(RAW_HEAD_HEADER),
        header_value(RAW_HEAD_HEADER, &hex(head.as_bytes()))?,
    );

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
    Ok(request)
}

fn header_value(name: &str, value: &str) -> Result<HeaderValue, UpstreamError> {
    HeaderValue::from_str(value).map_err(|_| UpstreamError {
        message: format!("invalid {name} header"),
    })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::pool::rewrite_head;

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
        let hex = request.headers()[RAW_HEAD_HEADER].to_str().unwrap();
        let mut hyper_head = format!("GET / HTTP/1.1\r\n{RAW_HEAD_HEADER}: {hex}\r\n\r\n");
        let (bytes, _) = rewrite_head(hyper_head.as_bytes());
        hyper_head = String::from_utf8(bytes).unwrap();
        hyper_head
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
