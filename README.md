<p align="center">
  <a href="http://opensource.hodfords.uk" target="blank"><img src="https://opensource.hodfords.uk/img/logo.svg" width="320" alt="Hodfords Logo" /></a>
</p>

# Rust API Gateway

[`api-gateway`](https://crates.io/crates/api-gateway) is a Rust port of the [`@hodfords/nestjs-api-gateway`](https://www.npmjs.com/package/@hodfords/nestjs-api-gateway) library (the Reference Gateway, version 12.4.2). It replaces that gateway in front of unchanged NestJS upstream services: it reads each service's OpenAPI document, builds routes from it, and forwards requests with your middleware and Redis-backed rate limiting.

What clients and upstream services observe is the same as with the Reference Gateway, apart from a short list of approved [Deviations](docs/parity/README.md#deviations). That claim is tested: 113 scenarios run against both gateways and compare the results.

**Status:** every feature area is implemented and every row of the [Parity Checklist](docs/parity/README.md) is closed (`done`, `deviation` or `n/a`).

## Key Features:

- **Centralized API Management**: The gateway is the single entry point for client applications. Each Upstream Service publishes an OpenAPI document; the gateway turns its operations into Routes, re-reads the document every 10 seconds, and sends each request to the service that owns its prefix.

- **Security and Authentication**: Proxy middleware authenticates a request (API keys, JWT, anything you write) and adds the headers the upstream trusts. Headers the gateway owns are removed from whatever the client sent, and request paths are normalised before any routing decision.

- **Rate Limiting and Throttling**: Four kinds of Bucket in Redis, shared by every gateway instance: per client address, per anonymous client address, per Identity, and per Route from the `x-rate-limits` extension.

- **Logging and Monitoring**: The library logs through the `tracing` crate. OpenTelemetry traces are exported over OTLP with the cargo feature `otel`.

- **WebSocket Support**: WebSocket upgrades are proxied to the Upstream Service, with their own middleware chain.

- **Error Handling and Fault Tolerance**: Upstream connections are pooled and bounded by timeouts. An Upstream Service that cannot be reached answers 503, a Redis that cannot be reached answers 500 within 2 seconds, and the rendering of gateway errors can be replaced.

- **Document endpoints and MCP**: `/documents` (Scalar API reference) and `/document-json` aggregate every service's document. With the cargo feature `mcp`, Routes are exposed as MCP tools at `POST /mcp`.

It does not add what the Reference Gateway lacks: load balancing, retries, circuit breaking, CORS handling, response caching or TLS termination.

## Usage

#### Requirements
The crate uses **edition 2024** and needs a recent stable Rust (1.88 at the least; the workspace pins `stable` in `rust-toolchain.toml`). It runs on **Tokio** and needs a **Redis** server when rate limiting is enabled.

| `api-gateway` | Reference Gateway |
|---|---|
| `0.1.x` | `@hodfords/nestjs-api-gateway` 12.4.2 |

#### Install
```shell
cargo add api-gateway
```
The package is `api-gateway` and it is imported as `api_gateway`. A typical `Cargo.toml`:
```toml
[dependencies]
api-gateway = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
http = "1"
serde_json = "1"
```
Add `features = ["mcp"]` for the MCP server and `features = ["otel"]` for OpenTelemetry tracing.

#### Configuration
Build the gateway from `GatewayOptions` and serve it. The three arguments of `GatewayOptions::new` are required; every other field has a default.
```rust
use api_gateway::Gateway;
use api_gateway::gateway::{DocumentOptions, GatewayOptions};
use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
use api_gateway::upstream::UpstreamService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut options = GatewayOptions::new(
        // apiServices
        vec![
            UpstreamService::new(
                "user-service",
                "http://localhost:4020/documents-json",
                "http://localhost:4020",
            )
            .with_direct_prefixes(["oauth", "oidc"]),
        ],
        // throttler: isEnable, globalIpRateLimit + TTL, globalCustomRateLimit + TTL
        RateLimitOptions::new(true, BucketLimit::new(120, 60.0), BucketLimit::new(60, 60.0)),
        // redis
        RedisOptions::new("localhost", 6379),
    );
    options.open_api_security_keys = vec!["auth-user-id".into()];
    options.open_api_security_api_keys = vec!["x-api-key".into()];
    options.exclude_headers = vec!["auth-user-id".into()];
    options.scalar_options = Some(serde_json::json!({ "showExtensions": false }));
    options.documents = DocumentOptions {
        enabled: true,
        hide_document_ids: vec![],
    };

    let gateway = Gateway::builder(options).build()?;

    let listener = api_gateway::server::bind_dual_stack(3000)?;
    gateway
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
```
There is no `RedisModule` or `ScheduleModule` to import: the gateway opens its own Redis connection and reloads the documents on its own task.

Things to know:

- `Gateway::router()` returns an `axum::Router` whose fallback is the proxy, so the application's own routes (`GatewayBuilder::routes`) take precedence. WebSocket upgrades are proxied only when the gateway is served with `Gateway::serve`.
- Startup does not wait for upstream documents. `Gateway::ready()` resolves once every service's first load has settled.
- Handlers are registered on the builder, where the NestJS library discovers decorators. Handlers of one kind run in ascending priority (`*_with_priority`, default 1), then in registration order.
- The gateway sends no `X-Powered-By` header, where the Reference Gateway sends `X-Powered-By: Express` (Deviation D11). `GatewayBuilder::x_powered_by(true)` turns it on.

A complete program is the example [`crates/rust-gateway/examples/quickstart.rs`](crates/rust-gateway/examples/quickstart.rs) (`cargo run -p api-gateway --example quickstart`).

#### Redis connection
`RedisOptions` takes the host and port. Besides those it accepts a database number, credentials and TLS settings:
```rust
use api_gateway::rate_limit::{RedisOptions, RedisTls, RedisTlsOptions};

let mut redis = RedisOptions::new("redis.example.com", 6380);
redis.db = Some(0);
redis.username = std::env::var("REDIS_USERNAME").ok(); // optional, Redis ACL user
redis.password = std::env::var("REDIS_PASSWORD").ok(); // optional
redis.tls = RedisTls::Default; // the reference's `tls: true`

// or the reference's `tls: { ca, cert, key, servername, rejectUnauthorized }`:
redis.tls = RedisTls::Custom(RedisTlsOptions {
    ca: Some(std::fs::read("ca.pem")?),
    server_name: Some("redis.internal".into()),
    ..RedisTlsOptions::default()
});
```
Use `RedisTls::Default` for a managed Redis with a publicly trusted certificate (`rediss://`). Use `RedisTls::Custom` when you need a
custom CA, client certificates (`cert` and `key`, PEM), or an explicit SNI name. `reject_unauthorized: false` disables certificate verification
and should only be used with self-signed certificates outside production.

The example binary reads the values from the environment, with the names the Reference Gateway's repository uses: `REDIS_HOST`, `REDIS_PORT`, `REDIS_DB`, `REDIS_USERNAME`, `REDIS_PASSWORD`,
`REDIS_TLS`, `REDIS_TLS_CA_FILE`, `REDIS_TLS_CERT_FILE`, `REDIS_TLS_KEY_FILE`, `REDIS_TLS_SERVERNAME` and
`REDIS_TLS_REJECT_UNAUTHORIZED`.

#### Custom Authentication Header
You can handle the authentication header by implementing `ProxyMiddleware`. `handle` is called for every request that matched a Route, before it is forwarded. It returns `Ok(true)` to let the request continue, `Ok(false)` to reject it with 403, or a `MiddlewareError` to answer with a status and body of its own (any other error answers 500).
```rust
use api_gateway::handler::{GatewayRequest, MiddlewareError, ProxyMiddleware, async_trait};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;

struct AuthenticationMiddleware;

#[async_trait]
impl ProxyMiddleware for AuthenticationMiddleware {
    async fn handle(
        &self,
        _route: &Route,
        _request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("auth-user-id", "123");
        Ok(true)
    }
}
```
```rust
let gateway = Gateway::builder(options)
    .proxy_middleware(AuthenticationMiddleware)
    .build()?;
```
Header names added by middleware are sent in kebab-case, so `authUserId` also reaches the upstream as `auth-user-id`.

The headers listed in `exclude_headers` are *gateway-owned*: the gateway deletes any copy of them that the caller
sends (for every request — regular, bypass, static and WebSocket — and for the `/mcp` request) before any
middleware runs, so the only value an upstream service can ever receive is the one your middleware adds with
`proxy_request.add_header(...)`. Put every identity/trust header your services rely on (e.g. `auth-user-id`) in that
list.

Request paths are normalised (dot segments such as `..`, `%2e%2e` and backslashes are resolved) before the
gateway decides which service, bypass prefix or route a request belongs to, so the path that is checked is always the
path the upstream receives. `bypass_route_prefixes` match on whole path segments: `/user-services/oidc` covers
`/user-services/oidc/...` but not `/user-services/oidc-admin/...`.

Similarly, you can handle WebSocket upgrades by implementing `WsProxyMiddleware`. It has the same contract, without a Route.
```rust
use api_gateway::handler::{GatewayRequest, MiddlewareError, WsProxyMiddleware, async_trait};
use api_gateway::request::ProxyRequest;

struct WsAuthenticationMiddleware;

#[async_trait]
impl WsProxyMiddleware for WsAuthenticationMiddleware {
    async fn handle(
        &self,
        _request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("auth-user-id", "123");
        Ok(true)
    }
}
```
Register it with `GatewayBuilder::websocket_middleware`.

#### Static File
You can declare which requests are for static files by implementing `StaticRequestPredicate`. `is_static` is called before the request is processed; a request for which any predicate returns `true` is forwarded with no Route lookup, no middleware and no rate limiting.

```rust
use api_gateway::handler::{BoxError, GatewayRequest, StaticRequestPredicate, async_trait};

struct StaticRequestMiddleware;

#[async_trait]
impl StaticRequestPredicate for StaticRequestMiddleware {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        let url = request.url();
        Ok(url.contains("/images/") || url.contains("/statics/"))
    }
}
```
Register it with `GatewayBuilder::static_request`.

#### Document
API Gateway will aggregate all subservices into one. With `options.documents.enabled = true` (off by default) you can access it by the link `http://gateway/documents`; the JSON of one service is at `http://gateway/document-json?type=<prefix>`.

#### Authenticate
The gateway does not verify tokens itself: your proxy middleware does, and then adds a header such as `auth-user-id` for the upstream. `route.is_bearer_auth` tells the middleware that the operation requires authentication (its `security` names `bearer` or one of `open_api_security_keys`); `route.is_api_key_auth` does the same for `open_api_security_api_keys`.

```rust
/// The verified user, for later middleware and the Identity resolver.
#[derive(Clone)]
struct AuthUserId(String);

struct AuthenticationMiddleware;

#[async_trait]
impl ProxyMiddleware for AuthenticationMiddleware {
    async fn handle(
        &self,
        route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        // Replace with your own token verification.
        let user_id = request.header("authorization").map(|_token| "123".to_string());
        match user_id {
            Some(user_id) => {
                proxy_request.add_header("auth-user-id", user_id.clone());
                request.extensions_mut().insert(AuthUserId(user_id));
                Ok(true)
            }
            None if route.is_bearer_auth => Err(MiddlewareError::json(
                http::StatusCode::UNAUTHORIZED,
                serde_json::json!({ "message": "Unauthorized" }),
            )),
            None => Ok(true),
        }
    }
}
```
> Subservices stay NestJS and do not change. To define a request that requires authentication, they keep using the decorator `Auth()`, which checks the header and adds the information to OpenAPI.
>
> In subservices, getting user information is eliminated. Instead you can just get the userId with decorator `@CurrentUserId() id: string` instead of decorator `@CurrentUser()`

```typescript
@Auth()
index(@CurrentUserId() id: string): string {
    return 'Hello word'
}
```

#### Rate limit
Per-Route limits are not configured in the gateway. Subservices declare them with the decorator from `@hodfords/nestjs-api-gateway-client`, which writes the `x-rate-limits` extension into their OpenAPI document, and the gateway reads it from there.

>`ApiRateLimit(limit: number, ttl: number, status?: number)`

> **Parameter:**
> - limit: number of requests
> - ttl: limited time request
> - status: *[optional]* limit requests by status, for example you want to limit the number of failed login attempts in 1 minute to 3 times: `@ApiRateLimit(3, 60, 401)`

```typescript
@ApiRateLimit(5, 60, 200)
@ApiRateLimit(30, 60 * 60, 200)
@ApiRateLimit(3, 60, 401)
index(): string {
    return 'Hello word'
}
```
A service that is not NestJS can publish the same extension on an operation by hand:
```json
"x-rate-limits": [
    { "limit": 5, "ttl": 60, "status": 200 },
    { "limit": 3, "ttl": 60, "status": 401 }
]
```
Each entry has its own Bucket here, where the Reference Gateway counts all the entries of a Route in one (Deviation D2).

#### Global rate limits
`RateLimitOptions` enforces these global limits:

- **`global_ip`** (`globalIpRateLimit` / `globalIpRateLimitTTL`) — counts **all** requests per client IP (see [IP normalisation](#ip-normalisation)), checked before the proxy middlewares (authentication) run. This is a DDoS / abuse baseline that does **not** use the Identity resolver's key and is **not** skipped when the resolver returns no key.
- **`anonymous_ip_limit` / `anonymous_ip_ttl`** *(optional)* — counts only **anonymous** requests per client IP. See [Anonymous IP limit](#anonymous-ip-limit).
- **`custom`** (`globalCustomRateLimit` / `globalCustomRateLimitTTL`) — bucketed by whatever the Identity resolver returns (or the client IP if no resolver is configured). Skipped entirely when the resolver returns no key.

A request is rejected with 429 as soon as any limit is exceeded. With `enabled: false` nothing is counted and Redis is never contacted. Static requests, bypass requests and WebSocket upgrades are never rate limited.

#### Anonymous IP limit
When many users share one public IP (corporate NAT, VPN egress, mobile carrier CGNAT), a single per-IP limit forces a trade-off. Set it low enough to stop anonymous brute force, and one busy office locks everyone out. Set it high enough for the office, and it no longer stops attacks. Run two IP buckets at the same time instead:

| Bucket | Counts | Typical use |
| --- | --- | --- |
| `global_ip` (all) | every request from the IP | high flood ceiling that a whole office fits under |
| `anonymous_ip_limit` (anonymous) | only unauthenticated requests from the IP | tight limit for public / pre-login traffic |

Authenticated traffic is bounded per user by `custom`. Requests rejected by a proxy middleware (returning `Ok(false)` or an error, e.g. a bad token) count as anonymous, since their identity was never established. The anonymous bucket is off unless both fields are set.

A request is authenticated only when the Identity resolver returns `Identity::authenticated(key)`. `Identity::anonymous(key)` (and having no resolver at all) is treated as anonymous.

```rust
use api_gateway::handler::{BoxError, IdentityContext, IdentityResolver, async_trait};
use api_gateway::rate_limit::{BucketLimit, Identity, RateLimitOptions};

let mut rate_limit = RateLimitOptions::new(
    true,
    BucketLimit::new(3000, 60.0), // all traffic per IP
    BucketLimit::new(240, 60.0),  // per user / identity
);
rate_limit.anonymous_ip_limit = Some(300); // anonymous traffic per IP
rate_limit.anonymous_ip_ttl = Some(60.0);

struct KeyResolver;

#[async_trait]
impl IdentityResolver for KeyResolver {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        // `AuthUserId` is set by your authentication ProxyMiddleware (see Authenticate).
        Ok(Some(match context.request.extensions().get::<AuthUserId>() {
            Some(AuthUserId(user_id)) => Identity::authenticated(format!("user:{user_id}")),
            None => Identity::anonymous(format!("ip:{}", context.ip)),
        }))
    }
}
```
Register the resolver with `GatewayBuilder::identity_resolver`.

> Only mark a request `authenticated` from state your own authentication middleware verified (never from a raw client header), otherwise a client can opt itself out of the anonymous limit.

#### IP normalisation
Every IP-based key (the all-traffic and anonymous IP buckets, and the IP fallback identity used by the custom and per-endpoint limits) uses a normalised client address:

- IPv4-mapped IPv6 addresses (`::ffff:203.0.113.7`) are reduced to the IPv4 address (`203.0.113.7`).
- IPv6 addresses are masked to their `/64` prefix (`2001:db8:1:2::abcd` → `2001:db8:1:2:0:0:0:0/64`). A /64 is the smallest block normally assigned to one subscriber, so a client cannot dodge a per-IP limit by rotating through its own addresses. Set `ipv6_subnet` (1–128) to change the prefix length; `128` keeps one bucket per address.

The client address is the connection's peer unless you say which proxies to trust, so set `trust_proxy` (the counterpart of Express `trust proxy`) when the gateway runs behind a load balancer:
```rust
use api_gateway::rate_limit::TrustProxy;

rate_limit.trust_proxy = TrustProxy::List(vec!["loopback".into(), "10.0.0.0/8".into()]);
rate_limit.ipv6_subnet = Some(56);
```
`TrustProxy::All` and `TrustProxy::Hops(n)` are the counterparts of `true` and a number. The normalised address is passed to the Identity resolver as `context.ip`; prefer it over `context.request.peer_addr()` when building IP-based keys.

#### Custom rate-limit key
By default the custom limit buckets requests by the client IP. To bucket by something else (authenticated user id, tenant id, API key, etc.), register an `IdentityResolver` (the reference's `keyResolver`). It receives `IdentityContext { request, route, ip }` (`ip` is the [normalised](#ip-normalisation) client address) and returns the `Identity` to use: `Identity::anonymous(key)` or `Identity::authenticated(key)` (see [Anonymous IP limit](#anonymous-ip-limit)).

The resolver runs after the proxy middleware, and gateway-owned headers are already removed from the request it sees. Read what your middleware attached to the request (`request.extensions()`) instead of a header.

**Skip the custom limit for selected requests** by returning `Ok(None)` (or an Identity whose key is `None`, empty or whitespace only). The gateway then bypasses both the custom global limit and any per-endpoint `@ApiRateLimit` rules without incrementing their counters. The all-traffic IP limit still applies, and so does the anonymous IP limit unless the result is `Identity { key: None, authenticated: true }`.

```rust
#[async_trait]
impl IdentityResolver for KeyResolver {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        if context.request.url() == "/healthz" {
            return Ok(None); // skip health checks
        }
        if context.route.operation_id.as_deref() == Some("InternalProbeController_ping") {
            return Ok(None); // skip by operation id
        }
        let key = match context.request.extensions().get::<AuthUserId>() {
            Some(AuthUserId(user_id)) => format!("user:{user_id}"),
            None => format!("ip:{}", context.ip),
        };
        Ok(Some(Identity::anonymous(key)))
    }
}
```

The same identity is used for the custom global limit and for any per-endpoint `@ApiRateLimit` rule. Resolvers are asynchronous. If the resolver returns an error, the request is answered with 500 (no silent allow, no silent deny). If no resolver is registered, the custom limit falls back to the normalised client IP (matching the IP limit's bucketing key).

## Coming from the NestJS library

| NestJS (`@hodfords/nestjs-api-gateway`) | Rust (`api_gateway`) |
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

The full mapping, with the names that were not ported and why, is in [docs/parity/10-public-api.md](docs/parity/10-public-api.md).

## The example binary

`rust-gateway-server` mirrors the sample application in the Reference Gateway's repository and reads the same environment variables (`APP_PORT`, `API_SERVICES` and the `REDIS_*` variables above). [`sample.rs`](crates/rust-gateway-server/src/sample.rs) has one handler of every kind and [`main.rs`](crates/rust-gateway-server/src/main.rs) wires them. It is an example, the target of the Conformance Tests, and the binary the benchmarks measure. It is not a supported deployment.

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

## Support:

If you encounter any issues, have questions, or need assistance with the API Gateway, please contact the development team

Thank you for using the API Gateway! Happy API management and development!

## License

[MIT](LICENSE)
