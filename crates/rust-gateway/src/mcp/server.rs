//! The MCP endpoint (MCP-01 to MCP-04): authentication, then `rmcp`'s Streamable HTTP
//! transport in stateless mode, with `tools/list` and `tools/call` served from the
//! [`ToolRegistry`](super::ToolRegistry) and the [`ToolExecutor`].

use std::borrow::Cow;
use std::convert::Infallible;
use std::fmt::Display;
use std::sync::Arc;

use bytes::Bytes;
use http_body::Body;
use http_body_util::{BodyExt, Full};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomRequest,
    CustomResult, ErrorCode, Implementation, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};

use super::executor::{BoxFuture, ToolExecutor};
use super::options::ServerInfo;
use crate::error::{DefaultErrorRenderer, ErrorRenderer};
use crate::handler::{GatewayRequest, McpAuthDecision, McpAuthentication, MiddlewareError};
use crate::request::normalize_request_url;

/// The response body type of the endpoint.
pub type McpBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// The body of the 401 answer when an authentication handler refuses (MCP-04).
pub const UNAUTHORIZED_BODY: &str = r#"{"error":"Unauthorized"}"#;

/// The body of the 500 answer when an authentication handler fails with an error that is
/// not an HTTP error (Nest's answer to an unhandled error). The default error renderer
/// renders it; see [`McpEndpoint::with_error_renderer`].
pub const INTERNAL_ERROR_BODY: &str = r#"{"statusCode":500,"message":"Internal server error"}"#;

/// The `rmcp` server handler: one per request in stateless mode.
#[derive(Clone)]
struct GatewayMcpServer {
    executor: Arc<ToolExecutor>,
    info: ServerInfo,
}

/// The protocol versions the reference's TypeScript SDK (1.32) supports, newest first. `rmcp`
/// also knows `2026-07-28`, which the reference does not; `initialize` negotiates within this
/// list, so both answer the same version for every requested one (MCP-02).
const REFERENCE_PROTOCOL_VERSIONS: [&str; 5] = [
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

fn reference_protocol_versions() -> Vec<ProtocolVersion> {
    REFERENCE_PROTOCOL_VERSIONS
        .iter()
        .filter_map(|v| serde_json::from_value(serde_json::Value::from(*v)).ok())
        .collect()
}

impl ServerHandler for GatewayMcpServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(reference_protocol_versions())
    }

    fn get_info(&self) -> ServerConfig {
        let mut config = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        );
        config.server_info = Implementation::new(self.info.name(), self.info.version());
        config
    }

    /// A method `rmcp` does not know: the reference's SDK answers `-32601` with the message
    /// `Method not found` (rmcp's default message is the method name).
    async fn on_custom_request(
        &self,
        _request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "Method not found",
            None,
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .executor
            .registry()
            .tools()
            .iter()
            .map(|tool| {
                Tool::new(
                    tool.name.clone(),
                    tool.description.clone(),
                    Arc::new(tool.input_schema.clone()),
                )
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let fallback;
        let parts = match context.extensions.get::<http::request::Parts>() {
            Some(parts) => parts,
            None => {
                fallback = http::Request::new(()).into_parts().0;
                &fallback
            }
        };
        let arguments = request.arguments.unwrap_or_default();
        let call = self.executor.call(&request.name, &arguments, parts);
        // OTEL-03: the tool call's client span is a child of the `/mcp` request's server span.
        #[cfg(feature = "otel")]
        let result = crate::otel::in_request_context(&parts.extensions, call).await;
        #[cfg(not(feature = "otel"))]
        let result = call.await;
        let mut response = CallToolResult::success(vec![ContentBlock::text(result.text)]);
        response.is_error = result.is_error.then_some(true);
        Ok(response.into())
    }
}

/// The `POST /mcp` endpoint (MCP-01 to MCP-04), as a tower service or through
/// [`handle`](Self::handle). The gateway mounts it on the paths the reference's Express route
/// matches: `/mcp` in any letter case, with or without one trailing slash, with any query
/// string, for `POST` only. Every other method goes to the proxy (MCP-01).
#[derive(Clone)]
pub struct McpEndpoint {
    service: Arc<StreamableHttpService<GatewayMcpServer, NeverSessionManager>>,
    authenticators: Arc<[Arc<dyn McpAuthentication>]>,
    renderer: Arc<dyn ErrorRenderer>,
}

impl McpEndpoint {
    /// An endpoint calling tools with `executor`, whose registry's options give the server
    /// info, behind `authenticators` (run in the order given: the gateway sorts them by
    /// ascending priority, CFG-19). Errors of the authenticators are rendered by the
    /// [`DefaultErrorRenderer`].
    pub fn new(
        executor: Arc<ToolExecutor>,
        authenticators: Vec<Arc<dyn McpAuthentication>>,
    ) -> Self {
        let handler = GatewayMcpServer {
            info: executor.registry().options().server_info.clone(),
            executor,
        };
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(false)
            .disable_allowed_hosts();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(NeverSessionManager::default()),
            config,
        );
        Self {
            service: Arc::new(service),
            authenticators: authenticators.into(),
            renderer: Arc::new(DefaultErrorRenderer),
        }
    }

    /// Renders the errors of the authentication handlers with `renderer` (REQ-25): an
    /// [`MiddlewareError::Http`] as [`GatewayError::Middleware`](crate::error::GatewayError),
    /// any other as [`GatewayError::Internal`](crate::error::GatewayError).
    pub fn with_error_renderer(mut self, renderer: Arc<dyn ErrorRenderer>) -> Self {
        self.renderer = renderer;
        self
    }

    /// Handles one `/mcp` request: the authentication handlers, then the MCP protocol.
    ///
    /// The handlers see the request as a [`GatewayRequest`] whose peer address is the
    /// [`axum::extract::ConnectInfo`] or [`std::net::SocketAddr`] extension of the request,
    /// if any. What they attach to it is put back in the request's extensions, where the
    /// tool calls find it.
    pub async fn handle<B>(&self, request: http::Request<B>) -> http::Response<McpBody>
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Display,
    {
        let (mut parts, body) = request.into_parts();
        if !self.authenticators.is_empty() {
            let mut gateway_request = gateway_request(&mut parts);
            for authenticator in self.authenticators.iter() {
                let refusal = match authenticator.authenticate(&mut gateway_request).await {
                    Ok(McpAuthDecision::Allow) => continue,
                    Ok(McpAuthDecision::Deny) => {
                        json_response(http::StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY)
                    }
                    Ok(McpAuthDecision::Respond(response)) => {
                        response.map(|body| Full::new(body).boxed())
                    }
                    Err(error) => {
                        if let MiddlewareError::Other(source) = &error {
                            tracing::error!(target: "rust_gateway::mcp", "MCP authentication failed: {source}");
                        }
                        let rendered = self.renderer.render(&error.into_gateway_error());
                        rendered.map(|body| Full::new(Bytes::from(body)).boxed())
                    }
                };
                return refusal;
            }
            parts.extensions = gateway_request.into_extensions();
        }
        self.service
            .handle(http::Request::from_parts(parts, body))
            .await
    }
}

/// The `/mcp` request as the authentication handlers see it. Its extensions are moved into
/// it and must be put back.
fn gateway_request(parts: &mut http::request::Parts) -> GatewayRequest {
    let peer = peer_addr(&parts.extensions);
    let target = parts.uri.to_string();
    let url = normalize_request_url(&target).unwrap_or(target);
    GatewayRequest::new(
        parts.method.clone(),
        parts.uri.clone(),
        url,
        parts.headers.clone(),
        peer,
    )
    .with_version(parts.version)
    .with_extensions(std::mem::take(&mut parts.extensions))
}

/// The client address of a request: axum's `ConnectInfo`, or a plain `SocketAddr`.
pub(crate) fn peer_addr(extensions: &http::Extensions) -> Option<std::net::SocketAddr> {
    extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0)
        .or_else(|| extensions.get::<std::net::SocketAddr>().copied())
}

impl std::fmt::Debug for McpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEndpoint")
            .field("authenticators", &self.authenticators.len())
            .finish_non_exhaustive()
    }
}

fn json_response(status: http::StatusCode, body: &'static str) -> http::Response<McpBody> {
    let mut response = http::Response::new(Full::new(Bytes::from_static(body.as_bytes())).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

impl<B> tower_service::Service<http::Request<B>> for McpEndpoint
where
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Display,
{
    type Response = http::Response<McpBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Infallible>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let endpoint = self.clone();
        Box::pin(async move { Ok(endpoint.handle(request).await) })
    }
}
