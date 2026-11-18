//! Tool calls (MCP-14 to MCP-20): the reference's `McpToolExecutorService`.
//!
//! The executor owns the order of a call and every result text. The parts that need the
//! server (rate limiting, the proxy middleware chain, the upstream HTTP client) sit behind
//! [`ToolCallBackend`], which the server implements.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use http::HeaderMap;
use serde_json::{Map, Value};

pub use super::call::UpstreamCall;
use super::call::build_upstream_call;
use super::registry::{ToolDefinition, ToolRegistry};
use crate::rate_limit::TooManyRequests;
use crate::request::{ProxyRequest, strip_excluded_headers};

/// A boxed future, as the backend's methods return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The text of a call the middleware disallowed (MCP-16).
pub const FORBIDDEN_TEXT: &str = "Request forbidden by gateway middleware";

/// What [`ToolCallBackend::authorize`] is given about a call.
#[derive(Debug, Clone, Copy)]
pub struct ToolCallContext<'a> {
    /// The tool; its [`ToolDefinition::route`] is the Route rate limits and middleware use.
    pub tool: &'a ToolDefinition,
    /// The `/mcp` request as received (its body already consumed). Rate limiting reads the
    /// client address and headers from here: the reference rate-limits before it removes
    /// `excludeHeaders`, so the Identity resolver sees them.
    pub request: &'a http::request::Parts,
    /// The `/mcp` request's headers with every `excludeHeaders` name removed: the headers
    /// the proxy middleware must see (MCP-16, REQ-17).
    pub middleware_headers: &'a HeaderMap,
    /// The tool arguments, which the middleware sees as the request body (MCP-16).
    pub arguments: &'a Map<String, Value>,
}

/// Why [`ToolCallBackend::authorize`] did not admit a call.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ToolCallRejection {
    /// A middleware returned false: [`FORBIDDEN_TEXT`] (MCP-16).
    #[error("{FORBIDDEN_TEXT}")]
    Forbidden,
    /// A Bucket is full: `Error executing <tool>: <message>` with
    /// [`TooManyRequests::message`] (MCP-19).
    #[error(transparent)]
    TooManyRequests(#[from] TooManyRequests),
    /// Anything else (a middleware error, a resolver failure, Redis): `Error executing
    /// <tool>: <message>` (MCP-19). For an HTTP error thrown by a middleware the reference's
    /// message is the body's `message` string, the string body itself, or `Http Exception`
    /// for an object body without one; for any other error, the error's message.
    #[error("{0}")]
    Failed(String),
}

/// The upstream's answer to a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResponse {
    /// The status code.
    pub status: u16,
    /// The whole body.
    pub body: Vec<u8>,
}

/// The upstream request failed before a response arrived. `message` becomes the result
/// text `Error executing <tool>: <message>` (MCP-19; undici's is `other side closed`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UpstreamError {
    /// The HTTP client's message.
    pub message: String,
}

/// What a tool call needs from the server.
pub trait ToolCallBackend: Send + Sync {
    /// Runs rate limiting, then the proxy middleware chain, in the reference's MCP order
    /// (MCP-15, MCP-16), and returns the headers the middleware added.
    ///
    /// 1. Rate limiting against the `/mcp` request (`context.request`), as the "Call order for
    ///    an MCP tool call" in [`crate::rate_limit`] describes: Global IP, then the Identity
    ///    (the resolver gets the `/mcp` request and the tool's Route and runs before any
    ///    middleware), Anonymous IP, Custom, and the Route Buckets keyed with method `POST`
    ///    (status-scoped ones checked, never counted). Skip all of it, resolver included,
    ///    when rate limiting is disabled.
    /// 2. The proxy middleware chain with the tool's Route, a request whose headers are
    ///    `context.middleware_headers` and whose body is `context.arguments`, and a fresh
    ///    [`ProxyRequest`]. The Anonymous IP Bucket is not counted when it rejects.
    fn authorize<'a>(
        &'a self,
        context: ToolCallContext<'a>,
    ) -> BoxFuture<'a, Result<ProxyRequest, ToolCallRejection>>;

    /// Sends `call` to the Upstream Service `service` over that service's connection pool
    /// (CFG-10, MCP-20) and reads the whole response. Redirects are not followed.
    fn send<'a>(
        &'a self,
        service: &'a str,
        call: UpstreamCall,
    ) -> BoxFuture<'a, Result<UpstreamResponse, UpstreamError>>;
}

/// The result of a tool call: one text content item, and whether it is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The text.
    pub text: String,
    /// `isError: true`. Successful results carry no `isError` field, as in the reference.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful result.
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// An error result.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }

    /// The result as the JSON-RPC `result` object the reference sends.
    pub fn to_json(&self) -> Value {
        let mut result = Map::new();
        result.insert(
            "content".into(),
            serde_json::json!([{ "type": "text", "text": self.text }]),
        );
        if self.is_error {
            result.insert("isError".into(), Value::Bool(true));
        }
        Value::Object(result)
    }
}

/// Executes tool calls: the reference's `McpToolExecutorService.executeTool`.
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    backend: Arc<dyn ToolCallBackend>,
    exclude_headers: Vec<String>,
}

impl ToolExecutor {
    /// An executor resolving tools in `registry`, with the configured `excludeHeaders`.
    pub fn new(
        registry: Arc<ToolRegistry>,
        backend: Arc<dyn ToolCallBackend>,
        exclude_headers: Vec<String>,
    ) -> Self {
        Self {
            registry,
            backend,
            exclude_headers,
        }
    }

    /// The registry tools are resolved in.
    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    /// Calls the tool `name` (MCP-14 to MCP-19):
    ///
    /// 1. An unknown name gives `Tool not found: <name>`.
    /// 2. [`ToolCallBackend::authorize`]; a rejection gives its result.
    /// 3. [`build_upstream_call`]; a path argument refused by Deviation D7 gives
    ///    `Error executing <name>: Invalid path argument path_<x>`.
    /// 4. [`ToolCallBackend::send`]. A status of 400 or above gives `Error executing <name>:
    ///    Downstream service returned <status>: <body>`; otherwise the body is the text,
    ///    decoded as UTF-8 with invalid bytes replaced.
    pub async fn call(
        &self,
        name: &str,
        arguments: &Map<String, Value>,
        request: &http::request::Parts,
    ) -> ToolResult {
        let Some(tool) = self.registry.tool(name) else {
            return ToolResult::error(format!("Tool not found: {name}"));
        };
        match self.execute(&tool, arguments, request).await {
            Ok(text) => ToolResult::success(text),
            Err(Failure::Forbidden) => ToolResult::error(FORBIDDEN_TEXT),
            Err(Failure::Error(message)) => {
                tracing::error!(target: "rust_gateway::mcp", "Tool execution failed: {name}: {message}");
                ToolResult::error(format!("Error executing {name}: {message}"))
            }
        }
    }

    async fn execute(
        &self,
        tool: &ToolDefinition,
        arguments: &Map<String, Value>,
        request: &http::request::Parts,
    ) -> Result<String, Failure> {
        let mut middleware_headers = request.headers.clone();
        strip_excluded_headers(&mut middleware_headers, &self.exclude_headers);
        let context = ToolCallContext {
            tool,
            request,
            middleware_headers: &middleware_headers,
            arguments,
        };
        let proxy_request = match self.backend.authorize(context).await {
            Ok(proxy_request) => proxy_request,
            Err(ToolCallRejection::Forbidden) => return Err(Failure::Forbidden),
            Err(ToolCallRejection::TooManyRequests(rejection)) => {
                return Err(Failure::Error(rejection.message().to_string()));
            }
            Err(ToolCallRejection::Failed(message)) => return Err(Failure::Error(message)),
        };
        let call = build_upstream_call(
            tool.method(),
            &tool.path,
            arguments,
            &proxy_request,
            &self.exclude_headers,
        )
        .map_err(|e| Failure::Error(e.to_string()))?;
        let response = self
            .backend
            .send(&tool.service, call)
            .await
            .map_err(|e| Failure::Error(e.message))?;
        let body = String::from_utf8_lossy(&response.body).into_owned();
        if response.status >= 400 {
            return Err(Failure::Error(format!(
                "Downstream service returned {}: {body}",
                response.status
            )));
        }
        Ok(body)
    }
}

impl std::fmt::Debug for ToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutor")
            .field("registry", &self.registry)
            .field("exclude_headers", &self.exclude_headers)
            .finish_non_exhaustive()
    }
}

enum Failure {
    Forbidden,
    Error(String),
}
