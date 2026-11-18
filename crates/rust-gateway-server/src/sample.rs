//! The reference sample's handlers and routes (EX-03, EX-06): `src/middleware/*.ts` and
//! `src/app.controller.ts` of the reference repository.

use axum::Router;
use axum::extract::OriginalUri;
use axum::routing::get;
use rust_gateway::handler::{
    BoxError, GatewayRequest, IdentityContext, IdentityResolver, McpAuthDecision,
    McpAuthentication, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, WsProxyMiddleware,
    async_trait,
};
use rust_gateway::rate_limit::Identity;
use rust_gateway::request::ProxyRequest;
use rust_gateway::route::Route;

use crate::protocol::AuthUserId;

/// Sets `auth-user-id: 123` on every Routed Request (`AuthenticationMiddleware`).
pub struct SampleAuthentication;

#[async_trait]
impl ProxyMiddleware for SampleAuthentication {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "123");
        request.extensions_mut().insert(AuthUserId("123".into()));
        Ok(true)
    }
}

/// Sets `auth-user-id: 123` on every WebSocket upgrade (`WsAuthenticationMiddleware`).
pub struct SampleWsAuthentication;

#[async_trait]
impl WsProxyMiddleware for SampleWsAuthentication {
    async fn handle(
        &self,
        _request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "123");
        Ok(true)
    }
}

/// URLs containing `/images/` or `/statics/` are Static Requests (`StaticRequestMiddleware`).
pub struct SampleStaticRequest;

#[async_trait]
impl StaticRequestPredicate for SampleStaticRequest {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        let url = request.url();
        Ok(url.contains("/images/") || url.contains("/statics/"))
    }
}

/// Accepts every MCP request (`McpAuthenticationMiddlewareImpl`).
pub struct SampleMcpAuthentication;

#[async_trait]
impl McpAuthentication for SampleMcpAuthentication {
    async fn authenticate(
        &self,
        _request: &mut GatewayRequest,
    ) -> Result<McpAuthDecision, MiddlewareError> {
        Ok(McpAuthDecision::Allow)
    }
}

/// The sample's `keyResolver`: no key for `/health`, `user:<id>` when a middleware set the
/// user, `ip:<address>` otherwise.
pub struct SampleIdentityResolver;

#[async_trait]
impl IdentityResolver for SampleIdentityResolver {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        if context.request.url() == "/health" {
            return Ok(None);
        }
        let key = match context.request.extensions().get::<AuthUserId>() {
            Some(AuthUserId(user)) if !user.is_empty() => format!("user:{user}"),
            _ => format!("ip:{}", context.ip),
        };
        Ok(Some(Identity::anonymous(key)))
    }
}

/// The sample's own routes (EX-06, CFG-18): `GET /` and `GET /oauth/metadata`, which take
/// precedence over the proxy even though `oauth` is a Direct Prefix.
pub fn routes() -> Router {
    Router::new()
        .route("/", get(|| async { "API Gateway" }))
        .route(
            "/oauth/metadata",
            get(|OriginalUri(uri): OriginalUri| async move { format!("Oauth: {uri}") }),
        )
}
