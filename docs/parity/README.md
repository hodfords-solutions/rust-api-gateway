# Parity Checklist

Every behaviour of the Reference Gateway that `rust-gateway` must reproduce, one row per behaviour. No Rust code is written for an area until its file here is approved.

- **Reference version:** `@hodfords/nestjs-api-gateway` 12.4.2, commit `0e97b52`. Line numbers are valid for that commit.
- **Reference paths** are relative to `libs/api-gateway/` in the reference repository unless they start with `libs/` or `src/`.
- Vocabulary is defined in [CONTEXT.md](../../CONTEXT.md).

## Areas

| File | Prefix | Area |
|---|---|---|
| [01-configuration.md](01-configuration.md) | `CFG` | Options, validation, startup |
| [02-openapi-routes.md](02-openapi-routes.md) | `OAS` | OpenAPI ingestion and the Route table |
| [03-request-pipeline.md](03-request-pipeline.md) | `REQ` | Sanitising, service selection, request classes, middleware, errors |
| [04-http-forwarding.md](04-http-forwarding.md) | `FWD` | Path rewrite, headers, bodies, upstream failures |
| [05-rate-limiting.md](05-rate-limiting.md) | `RL` | Buckets, Identity, rejection |
| [06-websocket.md](06-websocket.md) | `WS` | Upgrade handling and tunnelling |
| [07-documents.md](07-documents.md) | `DOC` | `/documents` and `/document-json` |
| [08-mcp.md](08-mcp.md) | `MCP` | MCP server, tool registry, tool execution |
| [09-opentelemetry.md](09-opentelemetry.md) | `OTEL` | Tracing (cargo feature `otel`) |
| [10-public-api.md](10-public-api.md) | `API`, `EX` | TypeScript export → Rust mapping, example binary |

## Columns

- **ID**: stable identifier, quoted in commits and test names.
- **Behaviour**: what a client, an upstream or an embedding application observes.
- **Reference**: where the Reference Gateway implements it.
- **Test**: the kind of test that must cover the row. The test name is appended when the test is written.
  - `C`: Conformance Test, run against both gateways.
  - `U`: unit test, usually ported from a reference spec.
  - `R`: Rust-only integration test, used where the two gateways differ by design or the reference has nothing to compare.
- **Status**: `todo`, `done`, `deviation` (done, and differs from the reference as approved), or `n/a` with a reason.

A row marked **confirm** states behaviour read from code or inferred from a dependency. Its Conformance Test is written first, and the row is corrected to whatever the Reference Gateway actually does.

## Deviations

Approved on 2026-11-18 (D10 and the wider D4 after the first Conformance Tests ran; D11, D13 and D14 once the areas they concern were implemented), D15 on 2026-11-19 (security finding SEC-01, [docs/security/attack-tests.md](../security/attack-tests.md)), and D16 on 2026-11-22 (security finding SEC-10). There is no D12: the exact bytes it would have excused (WS-15) were reproduced. Each is implemented as described, tested Rust-only, and the reference behaviour is recorded in the row.

| # | Deviation | Reference behaviour | Rows |
|---|---|---|---|
| D1 | A rejected WebSocket upgrade is answered with an error status and closed | Client socket is left open with no answer | WS-05 |
| D2 | Each Route rate limit has its own Bucket, scoped by Upstream Service | All limits on a Route share one counter; two services with the same method and pattern also share it | RL-17 |
| D3 | Path-item keys that are not one of the eight supported methods are skipped, and a document without `info` still loads | The whole document fails to load, so the service answers 503 | OAS-17, OAS-23 |
| D4 | Any method outside the eight supported ones answers 405; a Routed Request to a service whose document is not loaded answers 503 | 500 from an unhandled TypeError for methods Node accepts; a bare 400 from Node's parser for lower-case or unknown method tokens | REQ-08, REQ-15 |
| D5 | `/document-json` never crashes; unknown `type` answers 404 | 500 on missing `parameters`, missing `components`, unknown `type`, or a hidden operation on a multi-method path | DOC-10, DOC-14, DOC-15 |
| D6 | `/documents`: `?type=` selects a service; embedded JSON is escaped | `?type=` never matches; JSON is embedded unescaped in a script | DOC-04 |
| D7 | MCP path arguments are URL-encoded before substitution | Substituted raw, so `../x` can reach another upstream operation | MCP-17 |
| D8 | An MCP schema referenced more than once resolves every time, until a document's schemas have copied 200,000 JSON values; past that, a repeated reference resolves to `{}` as in the reference | Second and later references resolve to `{}` | MCP-12 |
| D9 | Query strings match the reference for common shapes only | Parsed by Express and re-serialised by undici | FWD-04 |
| D10 | When Redis cannot be reached, a request that needs a Bucket answers 500 within 2 seconds | The request waits about 73 s while the Redis client retries, then answers 500 | REQ-24 |
| D11 | The gateway does not send `X-Powered-By` unless the application turns it on (`GatewayBuilder::x_powered_by(true)`) | Every response carries `X-Powered-By: Express`, an Express default | FWD-15, REQ-23 |
| D13 | Edge cases of the MCP transport follow the Rust SDK `rmcp`: JSON-RPC batches, malformed or unacceptable requests, a response sent by the client, SSE framing, and some error codes. `initialize`, `tools/list`, `tools/call` and authentication match the reference | The TypeScript SDK's answers: batches accepted, JSON-RPC error bodies with status 400, 406 or 415, an `event: message` line in SSE | MCP-02 |
| D14 | Tracing sends one `traceparent` to the upstream, a WebSocket handshake span continues the client's trace and ends at the handshake, and only what the gateway instruments has spans | Auto-instrumentation: a second `traceparent` is appended when the client sent one, a WebSocket span starts a new trace and lasts until the tunnel closes, and Express, NestJS, DNS and Redis have spans | OTEL-02 to OTEL-04 |
| D15 | An upgrade request whose URL is a Route (and that is neither a Static nor a Bypass Request) passes that Route's checks before the WebSocket middleware: the Global IP Bucket, the proxy middleware, the Identity, the Custom and Route Buckets, as the same request without upgrade headers would. When the upstream does not upgrade, the client gets the status line only, not the body | An upgrade skips Route lookup, proxy middleware and every Bucket, so adding `Connection: Upgrade` and `Upgrade: websocket` to a `GET` bypasses the application's authentication and rate limits; the body of a non-101 upstream answer is relayed to the client | WS-04, WS-15, RL-24 |
| D16 | A `header_<x>` argument of an MCP tool call whose name differs only in letter case from a header the proxy middleware sets is not sent: the middleware's value is the only line of that name | Both lines are sent, the argument's first (`Authorization: <argument>`, then `authorization: <middleware>`), and a Node upstream keeps the first `authorization` and joins other headers, so a tool caller can override or add to an identity header the middleware owns | MCP-18 |

D9 is open-ended: each uncommon query shape found to differ is added to FWD-04 as it is discovered.

## Not ported

| Item | Reason |
|---|---|
| `libs/client` (`ApiRateLimit`, `ApiMcp`) | Runs inside NestJS upstream services, which keep using the TypeScript package (ADR 0001). Its output is the Extension Contract, covered by OAS-11 and OAS-15. |
| Decorator discovery (`getProviderByMetadata`), including "priority 0 is ignored" | NestJS reflection. Replaced by explicit registration (CFG-19). |
| DI tokens `API_GATEWAY_OPTION`, `REDIS_OPTION`, `MCP_OPTION`, `THROTTLER_OPTION`; `libraryPath` | NestJS dependency injection plumbing. |
| `rawBodyBufferHelper`, `STRIPE_SIGNATURE` | Dead code, not exported. |
| `enableWs` proxy option | Declared, never read. |
| `McpOption.path` | Declared, never read; the route is fixed at `POST /mcp`. |
| `OpenTelemetryOptions.minDuration` | Declared, never read. |
| `isReqUrlInWhitelist`, `WHITELIST_PATHS` | Only feed the `Location` rewrite, which has no effect (FWD-16). |
| `isValidUrl`, `getPathFromUrl` as public functions | Internal helpers in the port; URL handling is covered by REQ-02 and OAS-18. |
| Redis key names, value formats and the Lua script | Not part of the External Contract (ADR 0001). The algorithm's observable behaviour is covered in `RL`. |

## Reference specs

How each of the 23 reference spec files is carried over. "Port" means its cases become Rust unit tests for the listed rows.

| Reference spec (under `libs/`) | Cases | Carried over as |
|---|---|---|
| `api-gateway/index.spec.ts` | 3 | Port: RL-07, RL-08. Key-string assertions dropped (Redis keys are not contract). |
| `api-gateway/proxy/proxy-server.spec.ts` | 8 | Port: FWD-06 to FWD-11 |
| `api-gateway/proxy/proxy-socket.spec.ts` | 16 | Port: WS-06 to WS-15 |
| `api-gateway/restful/helpers/object.helper.spec.ts` | 6 | Port: REQ-21 |
| `api-gateway/restful/helpers/prefix.helper.spec.ts` | 9 | Port: REQ-06 |
| `api-gateway/restful/helpers/provider.helper.spec.ts` | 4 | Not ported (decorator discovery). Ordering is covered by CFG-19. |
| `api-gateway/restful/helpers/raw-body-buffer.helper.spec.ts` | 4 | Not ported (dead code) |
| `api-gateway/restful/helpers/request-sanitizer.helper.spec.ts` | 5 | Port: REQ-02, REQ-03 |
| `api-gateway/restful/helpers/string.helper.spec.ts` | 11 | Port: RL-21, OAS-18. `isValidUrl` cases dropped. |
| `api-gateway/restful/helpers/whitelist.helper.spec.ts` | 5 | Not ported (feeds the ineffective `Location` rewrite) |
| `api-gateway/restful/models/proxy-request.model.spec.ts` | 4 | Port: REQ-21, REQ-22 |
| `api-gateway/restful/services/open-api.service.spec.ts` | 21 | Port: OAS-04 to OAS-15, DOC-06 to DOC-11 |
| `api-gateway/restful/services/proxy.service.spec.ts` | 21 | Port: REQ-04 to REQ-14, FWD-01 to FWD-03. `Location` mutation cases become FWD-16. |
| `api-gateway/restful/services/request.service.spec.ts` | 9 | Port: REQ-09, REQ-17 |
| `api-gateway/restful/services/ws-request.service.spec.ts` | 5 | Port: WS-04 |
| `api-gateway/throttlers/helpers/ip.helper.spec.ts` | 10 | Port: RL-07, CFG-07 |
| `api-gateway/throttlers/services/throttler.service.spec.ts` | 9 | Port: RL-01, RL-09, RL-11 to RL-14 |
| `api-gateway/throttlers/services/throttler.service.anonymous-ip.spec.ts` | 11 | Port: RL-10, CFG-06 |
| `api-gateway/throttlers/services/throttler.service.ip-normalisation.spec.ts` | 4 | Port: RL-07, CFG-07 |
| `api-gateway/throttlers/services/throttler.service.router-limit.spec.ts` | 7 | Port: RL-15, RL-16, RL-19. Shared-counter expectations replaced by RL-17 (D2). |
| `api-gateway/mcp/services/mcp-tool-registry.service.spec.ts` | 19 | Port: MCP-06 to MCP-12 |
| `api-gateway/mcp/services/mcp-tool-executor.service.spec.ts` | 11 | Port: MCP-14 to MCP-19 |
| `client/helpers/api-extension.spec.ts` | 7 | Not ported (`libs/client`) |
