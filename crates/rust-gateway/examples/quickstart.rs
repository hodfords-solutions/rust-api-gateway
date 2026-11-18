//! A gateway for one Upstream Service, with a proxy middleware and Redis-backed rate
//! limiting. Run with `cargo run -p rust-gateway --example quickstart` (needs Redis on
//! `localhost:6379` and the upstream on `localhost:4020`).

use rust_gateway::Gateway;
use rust_gateway::gateway::GatewayOptions;
use rust_gateway::handler::{GatewayRequest, MiddlewareError, ProxyMiddleware, async_trait};
use rust_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
use rust_gateway::request::ProxyRequest;
use rust_gateway::route::Route;
use rust_gateway::upstream::UpstreamService;

struct Authentication;

#[async_trait]
impl ProxyMiddleware for Authentication {
    async fn handle(
        &self,
        route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        if route.is_bearer_auth && request.header("authorization").is_none() {
            return Err(MiddlewareError::json(
                http::StatusCode::UNAUTHORIZED,
                serde_json::json!({ "message": "Unauthorized" }),
            ));
        }
        // Sent to the upstream as `auth-user-id`.
        proxy_request.add_header("authUserId", "123");
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = GatewayOptions::new(
        vec![UpstreamService {
            prefix: "user-service".into(),
            doc_url: "http://localhost:4020/documents-json".into(),
            host: "http://localhost:4020".into(),
            direct_prefixes: vec![],
        }],
        RateLimitOptions::new(true, BucketLimit::new(60, 60.0), BucketLimit::new(60, 60.0)),
        RedisOptions::new("localhost", 6379),
    );
    // Headers only the gateway may set; a client's copy is removed.
    options.exclude_headers = vec!["auth-user-id".into()];

    let gateway = Gateway::builder(options)
        .proxy_middleware(Authentication)
        .build()?;

    let listener = rust_gateway::server::bind_dual_stack(3000)?;
    gateway
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
