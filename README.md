# rust-gateway

A Rust port of the [`@hodfords/nestjs-api-gateway`](https://www.npmjs.com/package/@hodfords/nestjs-api-gateway) library (the Reference Gateway, version 12.4.2). It replaces that gateway in front of unchanged NestJS upstream services: it reads each service's OpenAPI document, builds routes from it, and forwards requests with your middleware and Redis-backed rate limiting.

What clients and upstream services observe is the same as with the Reference Gateway, apart from a short list of approved [Deviations](docs/parity/README.md#deviations). That claim is tested: 113 scenarios run against both gateways and compare the results.

**Status:** every feature area is implemented. One checklist row is open (MCP-02, edge cases of the MCP transport). Not published to crates.io.

## What it does

- **Routing from OpenAPI.** Each Upstream Service publishes an OpenAPI document; the gateway turns its operations into Routes and re-reads the document every 10 seconds.
- **Request pipeline.** URL normalisation, Upstream Service selection by prefix, proxy middleware that can authenticate a request and add headers for the upstream.
- **HTTP forwarding.** Streaming in both directions, pooled upstream connections, the Reference Gateway's header and error behaviour.
- **Rate limiting.** Four kinds of Bucket in Redis: per client address, per anonymous client address, per Identity, and per Route from the `x-rate-limits` extension.
- **WebSocket proxying.**
- **Document endpoints.** `/documents` (Scalar API reference) and `/document-json`.
- **MCP server** (cargo feature `mcp`). Exposes Routes as MCP tools at `POST /mcp`.
- **OpenTelemetry tracing** (cargo feature `otel`).

It does not add what the Reference Gateway lacks: load balancing, retries, circuit breaking, CORS handling, response caching or TLS termination.

## Using the library

The crate is used from a git or path dependency until it is published. The gateway is built from options and handlers, then served:

```rust
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
```

This snippet is illustrative and is not compiled by the test suite. The compiled, tested equivalent is the example binary: [`crates/rust-gateway-server/src/sample.rs`](crates/rust-gateway-server/src/sample.rs) has one handler of every kind and [`main.rs`](crates/rust-gateway-server/src/main.rs) wires them.

Things to know:

- `Gateway::router()` returns an `axum::Router` whose fallback is the proxy, so the application's own routes (`GatewayBuilder::routes`) take precedence. WebSocket upgrades are proxied only when the gateway is served with `Gateway::serve`.
- Startup does not wait for upstream documents. `Gateway::ready()` resolves once every service's first load has settled.
- Middleware passes values to later middleware and to the Identity resolver through `GatewayRequest::extensions_mut()`.
- The gateway sends `X-Powered-By: Express`, as the Reference Gateway does. `GatewayBuilder::x_powered_by(false)` turns it off.

## Coming from the NestJS library

| NestJS (`@hodfords/nestjs-api-gateway`) | Rust (`rust_gateway`) |
|---|---|
| `ApiGatewayModule.forRoot(option)` | `Gateway::builder(GatewayOptions)`, then `build()`, then `serve(listener, shutdown)` or `router()` |
| `ApiServiceDetail` | `upstream::UpstreamService` |
| `RouterDetail` | `route::Route` |
| `RateLimit` | `route::RouteLimit` |
| `@ProxyMiddleware(priority)` + `ProxyMiddlewareHandler` | `handler::ProxyMiddleware`, registered with `GatewayBuilder::proxy_middleware` or `proxy_middleware_with_priority` |
| `@WsProxyMiddleware(priority)` + `WsProxyMiddlewareHandler` | `handler::WsProxyMiddleware`, registered with `websocket_middleware` |
| `@ProxyValidation(priority)` + `ProxyValidationHandler` | `handler::StaticRequestPredicate`, registered with `static_request` |
| `@McpAuthenticationMiddleware(priority)` + `McpAuthenticationMiddlewareHandler` | `handler::McpAuthentication`, registered with `mcp_authentication` |
| `ProxyRequest` (`addHeader`, `addHeaders`, `getKebabHeaders`) | `request::ProxyRequest` (`add_header`, `add_headers`, `kebab_headers`) |
| `OpenApiService` | `openapi::DocumentStore`, from `Gateway::documents()` |
| `TooManyRequestException` | `error::GatewayError::RateLimited` |
| `ThrottlerOption` | `rate_limit::RateLimitOptions` |
| `ThrottlerOption.keyResolver` | `handler::IdentityResolver`, set with `GatewayBuilder::identity_resolver` |
| Express `trust proxy` | `rate_limit::TrustProxy` in `RateLimitOptions::trust_proxy` |
| A NestJS exception filter | `error::ErrorRenderer`, set with `GatewayBuilder::error_renderer` |
| `normalizeIp`, `DEFAULT_IPV6_SUBNET` | `rate_limit::normalize_ip`, `rate_limit::DEFAULT_IPV6_SUBNET` |
| `ttlToHumanReadable` | `rate_limit::ttl_to_human_readable` |
| `McpOption` | `mcp::McpOptions` |
| `McpToolDefinition` | `mcp::ToolDefinition` |
| `McpToolRegistryService` | `Gateway::mcp_tools()` |
| `tracing()` from `@hodfords/nestjs-api-gateway-opentelemetry` | `otel::start(&OpenTelemetryOptions)` |
| `@hodfords/nestjs-api-gateway-client` (`ApiRateLimit`, `ApiMcp`) | Not ported: upstream services stay NestJS and keep using it |

Decorator discovery is replaced by explicit registration on the builder. The full mapping, with the names that were not ported and why, is in [docs/parity/10-public-api.md](docs/parity/10-public-api.md).

## The example binary

`rust-gateway-server` mirrors the sample application in the Reference Gateway's repository and reads the same environment variables (`APP_PORT`, `REDIS_HOST`, `REDIS_PORT`, `REDIS_DB`, `REDIS_USERNAME`, `REDIS_PASSWORD`, `REDIS_TLS` and the `REDIS_TLS_*` files). It is an example, the target of the Conformance Tests, and the binary the benchmarks measure. It is not a supported deployment.

```bash
cargo run -p rust-gateway-server
```

## Benchmarks

[benches/RESULTS.md](benches/RESULTS.md) holds the results and the machine they were taken on; [benches/README.md](benches/README.md) explains the method. Both gateways run in Docker with the same limits, the same configuration and the same JWT middleware, which the user confirmed on 2026-11-18 as representative of their production middleware.

## Layout

| Path | Contents |
|---|---|
| `crates/rust-gateway` | The library crate. |
| `crates/rust-gateway-server` | Example binary; also the Rust target of the Conformance Tests. |
| `conformance/` | The conformance harness: reference app, OpenAPI fixtures, mock upstream and scenarios. See [conformance/README.md](conformance/README.md). |
| `benches/` | End-to-end load test. Micro-benchmarks are in `crates/rust-gateway/benches/`. |
| `docs/parity/` | The Parity Checklist: every Reference Gateway behaviour, each tied to a test. |
| `docs/adr/` | Architecture decisions. |
| `CONTEXT.md` | Domain vocabulary. |

## Development

See [docs/development.md](docs/development.md) for running unit tests, the Conformance Tests against either gateway, and the working rules.

## License

[MIT](LICENSE)
