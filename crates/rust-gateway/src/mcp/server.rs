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

/// The response body type of the endpoint.
pub type McpBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

/// The body of the 401 answer when an authentication handler refuses (MCP-04).
pub const UNAUTHORIZED_BODY: &str = r#"{"error":"Unauthorized"}"#;

/// The body of the 500 answer when an authentication handler fails (Nest's answer to an
/// unhandled error).
pub const INTERNAL_ERROR_BODY: &str = r#"{"statusCode":500,"message":"Internal server error"}"#;

/// What an MCP authentication handler decided.
///
/// TEMPORARY: the server session defines the gateway's handler traits; this local
/// trait and its outcome stand in for the MCP one until then.
#[derive(Debug)]
pub enum McpAuthOutcome {
    /// Continue with the next handler, then the MCP protocol.
    Authenticated,
    /// Stop with 401 [`UNAUTHORIZED_BODY`].
    Unauthorized,
    /// Stop with this response (the reference handler wrote its own response and returned
    /// false).
    Respond(http::Response<Bytes>),
    /// The handler failed: stop with 500 [`INTERNAL_ERROR_BODY`].
    Failed(String),
}

/// An MCP authentication handler (the reference's `McpAuthenticationMiddlewareHandler`).
/// Handlers run in the order given to [`McpEndpoint::new`], which the application sorts by
/// ascending priority (MCP-04, CFG-19). A handler may add extensions to the request; the
/// tool-call backend sees them.
///
/// TEMPORARY: to be replaced by the trait the server session defines.
pub trait McpAuthentication: Send + Sync {
    /// Decides whether the `/mcp` request may proceed. The body has not been read.
    fn authenticate<'a>(
        &'a self,
        request: &'a mut http::request::Parts,
    ) -> BoxFuture<'a, McpAuthOutcome>;
}

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
        let result = self.executor.call(&request.name, &arguments, parts).await;
        let mut response = CallToolResult::success(vec![ContentBlock::text(result.text)]);
        response.is_error = result.is_error.then_some(true);
        Ok(response.into())
    }
}

struct Inner {
    service: StreamableHttpService<GatewayMcpServer, NeverSessionManager>,
    authenticators: Vec<Arc<dyn McpAuthentication>>,
}

/// The `POST /mcp` endpoint (MCP-01 to MCP-04), as a tower service or through
/// [`handle`](Self::handle). Mount it for `POST` only, on the paths the reference's Express
/// route matches: `/mcp` in any letter case, with or without one trailing slash, with any
/// query string. Every other method goes to the proxy (MCP-01).
#[derive(Clone)]
pub struct McpEndpoint {
    inner: Arc<Inner>,
}

impl McpEndpoint {
    /// An endpoint calling tools with `executor`, whose registry's options give the server
    /// info, behind `authenticators` (in that order).
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
            inner: Arc::new(Inner {
                service,
                authenticators,
            }),
        }
    }

    /// Handles one `/mcp` request: the authentication handlers, then the MCP protocol.
    pub async fn handle<B>(&self, request: http::Request<B>) -> http::Response<McpBody>
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Display,
    {
        let (mut parts, body) = request.into_parts();
        for authenticator in &self.inner.authenticators {
            match authenticator.authenticate(&mut parts).await {
                McpAuthOutcome::Authenticated => {}
                McpAuthOutcome::Unauthorized => {
                    return json_response(http::StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY);
                }
                McpAuthOutcome::Respond(response) => {
                    return response.map(|body| Full::new(body).boxed());
                }
                McpAuthOutcome::Failed(message) => {
                    tracing::error!(target: "rust_gateway::mcp", "MCP authentication failed: {message}");
                    return json_response(
                        http::StatusCode::INTERNAL_SERVER_ERROR,
                        INTERNAL_ERROR_BODY,
                    );
                }
            }
        }
        self.inner
            .service
            .handle(http::Request::from_parts(parts, body))
            .await
    }
}

impl std::fmt::Debug for McpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEndpoint")
            .field("authenticators", &self.inner.authenticators.len())
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
