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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{HeaderMap, Method, Request, Uri};
    use tower::ServiceExt as _;

    fn request(url: &str) -> GatewayRequest {
        GatewayRequest::new(
            Method::GET,
            Uri::from_static("/"),
            url,
            HeaderMap::new(),
            None,
        )
    }

    // EX-03: the sample's handlers.
    #[tokio::test]
    async fn ex_03_sample_handlers() {
        for (url, expected) in [
            ("/svc/images/a.png", true),
            ("/svc/statics/app.js", true),
            ("/svc/v1/users", false),
        ] {
            assert_eq!(
                SampleStaticRequest.is_static(&request(url)).await.unwrap(),
                expected,
                "{url}"
            );
        }
        let mut proxy_request = ProxyRequest::new();
        let mut req = request("/svc/v1/users");
        let route = rust_gateway::route::Route {
            method: rust_gateway::route::RouteMethod::Get,
            operation_id: None,
            description: String::new(),
            path: "/v1/users".into(),
            router_path: "/v1/users".into(),
            pattern: rust_gateway::route::pattern::PathPattern::compile("/v1/users").unwrap(),
            rate_limits: Vec::new(),
            is_bearer_auth: false,
            is_api_key_auth: false,
            extra: serde_json::Map::new(),
        };
        assert!(
            SampleAuthentication
                .handle(&route, &mut req, &mut proxy_request)
                .await
                .unwrap()
        );
        assert_eq!(
            proxy_request.kebab_headers(),
            [("auth-user-id".to_string(), "123".to_string())]
        );
        let mut ws_headers = ProxyRequest::new();
        assert!(
            SampleWsAuthentication
                .handle(&mut request("/svc/socket"), &mut ws_headers)
                .await
                .unwrap()
        );
        assert_eq!(ws_headers.kebab_headers(), proxy_request.kebab_headers());
        assert!(matches!(
            SampleMcpAuthentication
                .authenticate(&mut request("/mcp"))
                .await
                .unwrap(),
            McpAuthDecision::Allow
        ));
    }

    // EX-06: the sample's own routes.
    #[tokio::test]
    async fn ex_06_sample_routes() {
        let response = routes()
            .oneshot(Request::get("/oauth/metadata?x=1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"Oauth: /oauth/metadata?x=1");
    }
}
