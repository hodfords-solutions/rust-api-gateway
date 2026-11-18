# 01 Configuration and startup

The Reference Gateway is configured by one options object passed to `ApiGatewayModule.forRoot`. It reads no environment variables and no files. The port takes the same options through a builder.

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| CFG-01 | Upstream Services are a list; each has a Service Prefix, an OpenAPI document URL, a host origin, and optional Direct Prefixes. | `restful/types/api-service.type.ts:1-6` | U | todo |
| CFG-02 | `excludeHeaders` is a required list of gateway-owned header names. It drives REQ-03, REQ-17, WS-04, MCP-16, MCP-17 and DOC-08. | `types/api-gateway-option.type.ts:10` | U | todo |
| CFG-03 | `openApiSecurityKeys` is a required list of security-scheme names treated as bearer authentication (OAS-12, DOC-09). | `types/api-gateway-option.type.ts:11` | U | todo |
| CFG-04 | `openApiSecurityApiKeys` is an optional list of security-scheme names treated as API-key authentication (OAS-13). | `types/api-gateway-option.type.ts:12` | U | todo |
| CFG-05 | Rate-limit options are required: Global IP limit and TTL, Custom limit and TTL, an enable flag, and an optional Identity resolver. TTLs are in seconds. | `throttlers/types/throttler-option.type.ts:31-37` | U | todo |
| CFG-06 | The Anonymous IP limit and its TTL must be set together, and both must be positive finite numbers. Otherwise construction fails. | `throttlers/services/throttler.service.ts:46-58` | U | todo |
| CFG-07 | `ipv6Subnet` defaults to 64 and must be an integer from 1 to 128. Otherwise construction fails. | `throttlers/helpers/ip.helper.ts:3,73-77`; `throttlers/services/throttler.service.ts:59-60` | U | todo |
| CFG-08 | Document options: an enable flag and an optional list of operation ids to hide (DOC-10). | `restful/types/restful-option.type.ts:1-4` | U | todo |
| CFG-09 | Redis options: host, port, optional db, username, password, and TLS. TLS `true` means default TLS settings; an object supplies CA, client certificate, key, server name and whether to verify the certificate. | `redis/types/redis-option.type.ts:3-15`; `api-gateway.module.ts:19-31` | R | todo |
| CFG-10 | Upstream connection-pool options apply to every Upstream Service. The port maps the maximum number of connections per service; other undici-specific fields have no equivalent. **confirm** which undici defaults (header and body timeouts, keep-alive) are observable and must be matched. | `types/api-gateway-option.type.ts:18`; `proxy/proxy-server.ts:24` | R | todo |
| CFG-11 | `scalarOptions` is an optional JSON object passed through to the document page (DOC-03). | `types/api-gateway-option.type.ts:17` | U | todo |
| CFG-12 | `bypassRoutePrefixes` is an optional list of path prefixes (REQ-12). | `types/api-gateway-option.type.ts:19` | U | todo |
| CFG-13 | MCP options are optional. The MCP endpoint exists only when `mcp.enabled` is true. | `api-gateway.module.ts:38`; `mcp/types/mcp-option.type.ts:18-30` | C | todo |
| CFG-14 | The document endpoints exist only when documents are enabled. When disabled, `/documents` and `/document-json` fall through to the proxy. | `restful/restful.module.ts:19` | C | todo |
| CFG-15 | Configuration is fixed after start. Only Route tables and MCP tools change at runtime, and only from upstream documents. | `api-gateway.module.ts:16-58` | none | n/a: a design constraint, nothing to run |
| CFG-16 | Startup does not wait for upstream documents. A readiness signal resolves once every service's first load has settled, whether it succeeded or failed. | `restful/services/proxy.service.ts:64-87`; `restful/services/open-api.service.ts:30-45` | R | todo |
| CFG-17 | Rate limiting keeps working after Redis restarts or the connection drops and returns. | `throttlers/services/throttler.service.ts:63-72` | R | todo |
| CFG-18 | An application can register its own HTTP routes alongside the gateway; they take precedence over the proxy catch-all. | `restful/controllers/proxy.controller.ts:9`; `src/app.controller.ts:12-34` | R | todo |
| CFG-19 | Four kinds of handler can be registered, any number of each: proxy middleware, WebSocket middleware, static-request predicate, MCP authentication. Each registration has a priority (default 1); handlers of a kind run in ascending priority, and equal priorities keep registration order. | `restful/decorators/*.ts`; `mcp/decorators/mcp-authentication-middleware.decorator.ts`; `restful/helpers/provider.helper.ts:4-21` | U | todo |
