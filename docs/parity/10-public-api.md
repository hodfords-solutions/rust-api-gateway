# 10 Public API and example binary

## TypeScript export → Rust

Every name exported from `libs/api-gateway/index.ts`, and what an application uses instead in `rust-gateway`. Rust names follow [CONTEXT.md](../../CONTEXT.md) and are proposals until the first implementation session fixes them; the finished table is copied into the README.

| ID | TypeScript export | Rust counterpart | Reference | Status |
|---|---|---|---|---|
| API-01 | `ApiGatewayModule.forRoot(option)` | A gateway builder that takes the same options and yields a service the application mounts | `api-gateway.module.ts:16-58` | todo |
| API-02 | `ApiServiceDetail` | `UpstreamService` | `restful/types/api-service.type.ts:1-6` | todo |
| API-03 | `RouterDetail` | `Route` | `restful/types/router-path.type.ts:7-17` | todo |
| API-04 | `RateLimit` | `RouteLimit` | `restful/types/router-path.type.ts:1-5` | todo |
| API-05 | `RouterPathType` | Not public; the Route table is internal | `restful/types/router-path.type.ts:19-21` | todo |
| API-06 | `@ProxyMiddleware(priority)` + `ProxyMiddlewareHandler` | A proxy-middleware trait registered on the builder with a priority. It returns allow, deny, or an error carrying a status and body. | `restful/decorators/proxy-middleware.decorator.ts`; `restful/interfaces/proxy-middleware.interface.ts:5-11` | todo |
| API-07 | `@WsProxyMiddleware(priority)` + `WsProxyMiddlewareHandler` | A WebSocket-middleware trait, registered the same way | `restful/decorators/ws-proxy-middleware.decorator.ts`; `restful/interfaces/ws-proxy-middleware.interface.ts:4-6` | todo |
| API-08 | `@ProxyValidation(priority)` + `ProxyValidationHandler` | A static-request predicate trait, registered the same way | `restful/decorators/proxy-validation.decorator.ts`; `restful/interfaces/proxy-validation.interface.ts:3-5` | todo |
| API-09 | `@McpAuthenticationMiddleware(priority)` + `McpAuthenticationMiddlewareHandler` | An MCP-authentication trait, registered the same way | `mcp/decorators/mcp-authentication-middleware.decorator.ts`; `mcp/interfaces/mcp-authentication-middleware.interface.ts:3-5` | todo |
| API-10 | `PROXY_MIDDLEWARE`, `WS_PROXY_MIDDLEWARE`, `PROXY_VALIDATION`, `MCP_AUTHENTICATION_MIDDLEWARE` | Not ported (metadata keys) | `restful/decorators/*.ts` | n/a |
| API-11 | `ProxyRequest` (`headers`, `addHeader`, `addHeaders`, `getKebabHeaders`) | `ProxyRequest` with the same operations (REQ-21, REQ-22) | `restful/models/proxy-request.model.ts:3-17` | todo |
| API-12 | `OpenApiService` (`apiDocs`, `originDocs`, `ready`, `getRouterDetail`, `getDocument`) | A read-only handle to loaded documents and Routes, with the readiness signal of CFG-16 | `restful/services/open-api.service.ts:16-45` | todo |
| API-13 | `TooManyRequestException` | The rate-limit variant of the gateway error type (RL-20, RL-21) | `throttlers/exceptions/too-many-request.exception.ts:3-7` | todo |
| API-14 | `ThrottlerOption` | Rate-limit options on the builder (CFG-05 to CFG-07) | `throttlers/types/throttler-option.type.ts:31-52` | todo |
| API-15 | `ThrottlerKeyResolver`, `ThrottlerKeyResolverContext`, `ThrottlerKeyResolverResult`, `ThrottlerIdentity` | An Identity-resolver trait or closure returning an optional `Identity { key, authenticated }` (RL-11, RL-12) | `throttlers/types/throttler-option.type.ts:4-29` | todo |
| API-16 | `normalizeIp`, `DEFAULT_IPV6_SUBNET` | `normalize_ip`, `DEFAULT_IPV6_SUBNET` (RL-07, RL-08) | `index.ts:15` | todo |
| API-17 | `ttlToHumanReadable` | `ttl_to_human_readable` (RL-21, RL-23) | `restful/helpers/string.helper.ts:33-46` | todo |
| API-18 | `isValidUrl`, `getPathFromUrl` | Not ported as public functions | `restful/helpers/string.helper.ts:6-26` | n/a |
| API-19 | `McpOption`, `McpParameterFilterContext`, `McpToolNameContext` | MCP options on the builder, with the three callbacks (MCP-06, MCP-07, MCP-10) | `mcp/types/mcp-option.type.ts:3-30` | todo |
| API-20 | `McpToolDefinition` | `McpTool` | `mcp/types/mcp-tool.type.ts:3-11` | todo |
| API-21 | `McpModule`, `McpToolRegistryService`, `McpServerService`, `McpToolExecutorService` | A read-only handle to the tool list. The server and executor are internal. | `mcp/index.ts:1-9` | todo |
| API-22 | `MCP_OPTION` | Not ported (DI token) | `mcp/constants/mcp.constant.ts:1` | n/a |
| API-23 | `MAX_TOOL_NAME_LENGTH` | `MAX_TOOL_NAME_LENGTH` (MCP-08) | `mcp/constants/mcp.constant.ts:7` | todo |

## Example binary `rust-gateway-server`

An example only (Q11), mirroring `src/` of the reference. It is also the target of the Conformance Tests and benchmarks.

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| EX-01 | Reads the same environment variables as the reference sample: `APP_PORT` (default 3000), `REDIS_HOST` (default `localhost`), `REDIS_PORT` (default 6379), `REDIS_DB` (default 0), `REDIS_USERNAME`, `REDIS_PASSWORD`, `REDIS_TLS`, `REDIS_TLS_CA_FILE`, `REDIS_TLS_CERT_FILE`, `REDIS_TLS_KEY_FILE`, `REDIS_TLS_SERVERNAME`, `REDIS_TLS_REJECT_UNAUTHORIZED`. | `src/config/env.config.ts:12-51` | R | todo |
| EX-02 | Upstream Services are defined in code in the reference sample (`user-service` at `http://localhost:4020` with Direct Prefixes `oauth` and `oidc`). The example additionally accepts them from an environment variable so tests and benchmarks can point it at mock upstreams. | `src/config/env.config.ts:31-38` | R | todo |
| EX-03 | Registers one handler of each kind, equivalent to the reference sample: a proxy middleware and a WebSocket middleware that set the header `auth-user-id`, a static predicate for URLs containing `/images/` or `/statics/`, and an MCP authentication handler that accepts everything. | `src/middleware/*.ts` | R | todo |
| EX-04 | Uses the reference sample's settings: exclude headers `auth-user-id` and `permission-in-any-guard`; bearer key `auth-user-id`; API key `x-api-key`; all four limits at 60 per 60 s; documents and MCP enabled; trusted proxies `loopback`, `linklocal`, `uniquelocal`; 100 connections per Upstream Service. | `src/app.module.ts:16-55`; `src/main.ts:23` | R | todo |
| EX-05 | A second, representative middleware set for benchmarks: verify a JWT bearer token, set `auth-user-id`, and resolve the Identity as the user id when authenticated or the client address otherwise. The same logic is added to the reference app used in benchmarks. This reflects an assumption about production middleware that has not been confirmed. | none | R | todo |
| EX-06 | Serves application routes of its own next to the gateway, like the reference sample's `/` and `/oauth/metadata`, to demonstrate CFG-18. | `src/app.controller.ts:12-34` | R | todo |
