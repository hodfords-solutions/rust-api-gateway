//! The test middleware protocol of `conformance/README.md`, implemented as
//! `conformance/reference/src/protocol.ts` implements it for the reference target.

use std::cmp::Ordering;

use http::{Response, StatusCode};
use rust_gateway::error::{JSON_UTF8, js_json};
use rust_gateway::handler::{
    BoxError, GatewayRequest, IdentityContext, IdentityResolver, McpAuthDecision,
    McpAuthentication, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, WsProxyMiddleware,
    async_trait,
};
use rust_gateway::rate_limit::Identity;
use rust_gateway::request::{NodeHeaderValue, ProxyRequest, node_incoming_headers};
use rust_gateway::route::Route;
use serde_json::{Map, Value, json};

/// The value a middleware attaches to the request for `x-test-auth-user` (REQ-20): the
/// reference sets `request.authUserId`.
#[derive(Debug, Clone)]
pub struct AuthUserId(pub String);

/// A request header as `request.headers[name]` holds it in Node: lines joined with `, `
/// (the protocol's headers are never among those Node keeps only once).
fn header(request: &GatewayRequest, name: &str) -> Option<String> {
    node_incoming_headers(request.headers())
        .into_iter()
        .find(|(n, _)| n == name)
        .and_then(|(_, value)| match value {
            NodeHeaderValue::Single(bytes) => Some(bytes),
            NodeHeaderValue::Multiple(lines) => lines.into_iter().next(),
        })
        .map(|bytes| bytes.iter().map(|&b| char::from(b)).collect())
}

/// `JSON.stringify` with every character from U+007F escaped as `\uXXXX` (UTF-16 units).
fn ascii_json(value: &Value) -> String {
    let mut out = String::new();
    for c in js_json(value).chars() {
        if u32::from(c) >= 0x7f {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// JavaScript's default sort order: by UTF-16 code units.
fn js_order(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn describe_route(route: &Route) -> Value {
    let mut extra_keys: Vec<&String> = route.extra.keys().collect();
    extra_keys.sort_by(|a, b| js_order(a, b));
    json!({
        "operationId": route.operation_id,
        "description": route.description,
        "path": route.path,
        "routerPath": route.router_path,
        "isBearerAuth": route.is_bearer_auth,
        "isApiKeyAuth": route.is_api_key_auth,
        "rateLimits": route.rate_limits,
        "extraKeys": extra_keys,
    })
}

/// The error a handler throws for `x-test-middleware: throw`: status `x-test-error-status`
/// (default 401), body `x-test-error-body` parsed as JSON, or as a string when it is not.
fn protocol_error(request: &GatewayRequest) -> (StatusCode, Result<Value, String>) {
    let status = header(request, "x-test-error-status")
        .and_then(|s| s.trim().parse::<u16>().ok())
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::UNAUTHORIZED);
    let raw = header(request, "x-test-error-body")
        .unwrap_or_else(|| r#"{"message":"test middleware error"}"#.to_string());
    let body = serde_json::from_str::<Value>(&raw).map_err(|_| raw);
    (status, body)
}

/// A header value as the reference's `addHeaders` passes it to undici: strings as they are,
/// other JSON values as JavaScript writes them.
fn header_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        Value::Object(_) => Some("[object Object]".to_string()),
        Value::Array(items) => Some(
            items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => String::new(),
                    other => js_json(other),
                })
                .collect::<Vec<_>>()
                .join(","),
        ),
        other => Some(js_json(other)),
    }
}

/// The shared proxy and WebSocket middleware (`runMiddleware`).
fn run_middleware(
    request: &mut GatewayRequest,
    proxy_request: &mut ProxyRequest,
    route: Option<&Route>,
) -> Result<bool, MiddlewareError> {
    if let Some(user) = header(request, "x-test-auth-user") {
        proxy_request.add_header("authUserId", user.clone());
        request.extensions_mut().insert(AuthUserId(user));
    }
    if let Some(extra) = header(request, "x-test-add-headers") {
        let object: Map<String, Value> =
            serde_json::from_str(&extra).map_err(MiddlewareError::other)?;
        for (name, value) in object {
            if let Some(value) = header_value(&value) {
                proxy_request.add_header(name, value);
            }
        }
    }
    if let Some(route) = route
        && header(request, "x-test-echo-route").as_deref() == Some("1")
    {
        proxy_request.add_header("x-test-route", ascii_json(&describe_route(route)));
    }
    match header(request, "x-test-middleware")
        .as_deref()
        .unwrap_or("allow")
    {
        "allow" => Ok(true),
        "deny" => Ok(false),
        "throw" => {
            let (status, body) = protocol_error(request);
            Err(match body {
                Ok(value) => MiddlewareError::json(status, value),
                Err(text) => MiddlewareError::text(status, text),
            })
        }
        "crash" => Err(MiddlewareError::other("test middleware crash")),
        other => Err(MiddlewareError::other(format!(
            "unknown x-test-middleware mode: {other}"
        ))),
    }
}

/// The proxy middleware.
pub struct TestProxyMiddleware;

#[async_trait]
impl ProxyMiddleware for TestProxyMiddleware {
    async fn handle(
        &self,
        route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        run_middleware(request, proxy_request, Some(route))
    }
}

/// The WebSocket middleware.
pub struct TestWsMiddleware;

#[async_trait]
impl WsProxyMiddleware for TestWsMiddleware {
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        run_middleware(request, proxy_request, None)
    }
}

/// The static-request predicate: `x-test-static: 1`.
pub struct TestStaticRequest;

#[async_trait]
impl StaticRequestPredicate for TestStaticRequest {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        Ok(header(request, "x-test-static").as_deref() == Some("1"))
    }
}

/// The Identity resolver installed by `throttler.keyResolver: "protocol"`.
pub struct TestIdentityResolver;

#[async_trait]
impl IdentityResolver for TestIdentityResolver {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        let request = context.request;
        if header(request, "x-test-identity-throw").as_deref() == Some("1") {
            return Err("test identity resolver failure".into());
        }
        let auth_user = request
            .extensions()
            .get::<AuthUserId>()
            .map(|user| user.0.clone())
            .filter(|user| !user.is_empty());
        let mut from_user = false;
        let key = if header(request, "x-test-identity-none").as_deref() == Some("1") {
            None
        } else if let Some(key) = header(request, "x-test-identity") {
            Some(key)
        } else if let Some(user) = auth_user {
            from_user = true;
            Some(format!("user:{user}"))
        } else {
            Some(context.ip.to_string())
        };
        let authenticated = match header(request, "x-test-identity-authenticated") {
            Some(value) => value == "true",
            None => from_user,
        };
        Ok(Some(Identity { key, authenticated }))
    }
}

/// The MCP authentication handler.
pub struct TestMcpAuthentication;

#[async_trait]
impl McpAuthentication for TestMcpAuthentication {
    async fn authenticate(
        &self,
        request: &mut GatewayRequest,
    ) -> Result<McpAuthDecision, MiddlewareError> {
        match header(request, "x-test-mcp-auth")
            .as_deref()
            .unwrap_or("allow")
        {
            "allow" => Ok(McpAuthDecision::Allow),
            "deny" => Ok(McpAuthDecision::Deny),
            "respond" => {
                // `response.status(status).type('application/json').send(body)`: the body is
                // sent as given.
                let status = header(request, "x-test-error-status")
                    .and_then(|s| s.trim().parse::<u16>().ok())
                    .and_then(|s| StatusCode::from_u16(s).ok())
                    .unwrap_or(StatusCode::IM_A_TEAPOT);
                let body = header(request, "x-test-error-body")
                    .unwrap_or_else(|| r#"{"message":"test mcp authentication response"}"#.into());
                let mut response = Response::new(body.into());
                *response.status_mut() = status;
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static(JSON_UTF8),
                );
                Ok(McpAuthDecision::Respond(response))
            }
            other => Err(MiddlewareError::other(format!(
                "unknown x-test-mcp-auth mode: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_json_escapes_like_the_reference() {
        assert_eq!(
            ascii_json(&json!({"a": "é\u{1F600}", "b": 1.0})),
            r#"{"a":"é😀","b":1}"#
        );
    }

    #[test]
    fn extra_keys_sort_by_utf16_units() {
        let mut keys = vec!["b", "a", "\u{FF21}", "\u{1F600}", "B"];
        keys.sort_by(|a, b| js_order(a, b));
        assert_eq!(keys, ["B", "a", "b", "\u{1F600}", "\u{FF21}"]);
    }
}
