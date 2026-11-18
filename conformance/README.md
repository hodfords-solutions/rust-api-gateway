# Conformance harness

Conformance Tests run the same black-box scenario against two **targets** and compare what clients and upstreams observe:

- `reference`: the NestJS app in [`reference/`](reference/), which embeds `@hodfords/nestjs-api-gateway` from npm (the Reference Gateway).
- `rust`: `rust-gateway-server`, the example binary of this repository.

How to run everything is in [docs/development.md](../docs/development.md). This file defines the two things both targets must implement identically: the **configuration file** and the **test middleware protocol**.

| Path | What it is |
|---|---|
| `reference/` | Reference target. Lockfile committed; bumped weekly by `.github/workflows/reference-bump.yml`. |
| `fixture-generator/` | NestJS service that writes the OpenAPI fixtures. Run once; output committed. |
| `fixtures/` | The generated OpenAPI documents served by mock upstreams. |
| `harness/` | Rust crate `conformance`: mock upstream (library and `mock-upstream` binary), raw client, runner, scenarios in `tests/conformance/`. |
| `rust-implemented.txt` | Checklist IDs the `rust` target is held to. |
| `docker-compose.yml` | Redis for scenarios, on host port 6399, with 128 databases. |

## Configuration file

The runner writes one JSON file per gateway start and passes its path in `GATEWAY_CONFIG`. Field names and meanings are the Reference Gateway's `ApiGatewayOption`, plus three harness fields. Source of truth: [`harness/src/config.rs`](harness/src/config.rs) and [`reference/src/config.ts`](reference/src/config.ts).

```json
{
  "port": 41234,
  "trustProxy": ["loopback", "linklocal", "uniquelocal"],
  "apiServices": [
    { "prefix": "svc", "docUrl": "http://127.0.0.1:5000/openapi.json", "host": "http://127.0.0.1:5000", "directPrefixes": ["oauth"] }
  ],
  "excludeHeaders": ["auth-user-id", "permission-in-any-guard"],
  "openApiSecurityKeys": ["auth-user-id"],
  "openApiSecurityApiKeys": ["x-api-key"],
  "throttler": {
    "globalIpRateLimit": 60, "globalIpRateLimitTTL": 60,
    "globalCustomRateLimit": 60, "globalCustomRateLimitTTL": 60,
    "isEnable": true,
    "anonymousIpRateLimit": 10, "anonymousIpRateLimitTTL": 60,
    "ipv6Subnet": 64,
    "keyResolver": "protocol"
  },
  "restful": { "isEnableDocument": true, "hideDocumentIds": [] },
  "scalarOptions": { "showExtensions": true },
  "redis": { "host": "127.0.0.1", "port": 6399, "db": 7, "username": null, "password": null, "tls": false },
  "pool": { "connections": 100, "pipelining": 1 },
  "bypassRoutePrefixes": ["svc/public"],
  "mcp": { "enabled": true, "allowedServices": ["svc"], "allowedOperations": [], "serverInfo": { "name": "x", "version": "1" } }
}
```

| Field | Meaning |
|---|---|
| `port` | Listen port. Like the reference sample, the target listens on all addresses with no host argument (Node: dual-stack `::`). |
| `trustProxy` | Express `trust proxy` value. Absent means the sample's `["loopback", "linklocal", "uniquelocal"]`. |
| `throttler.keyResolver` | `"protocol"` installs the protocol Identity resolver below. Absent means no resolver (Identity is the client address). |
| `redis.tls` | `true`, `false`, or `{ caFile, certFile, keyFile, servername, rejectUnauthorized }`. |
| `pool` | Passed to undici's `Pool` unchanged (CFG-10). The Rust target maps what it can. |
| everything else | As in `ApiGatewayOption`. |

The MCP callbacks (`filter`, `parameterFilter`, `toolName`) are functions and cannot be expressed in the file; a scenario that needs them must add a protocol for them first.

Environment variables override the file the way the reference sample reads them (EX-01): `APP_PORT`, `REDIS_HOST`, `REDIS_PORT`, `REDIS_DB`.

## Application route

Both targets serve `GET /__conformance/ready`. It answers once every Upstream Service's first document load has settled (CFG-16) with `{"services": [<prefixes with a loaded document>]}`, in load order. The runner polls it before a scenario starts.

## Test middleware protocol

Each target registers exactly one handler of each kind (CFG-19), all at priority 1. They do nothing unless a request carries the headers below. Header names are matched case-insensitively and the first value is used. The `x-test-*` headers are ordinary client headers, so they are forwarded upstream like any other.

### Proxy middleware and WebSocket middleware

The same rules apply to both. Steps run in this order.

| Header | Effect |
|---|---|
| `x-test-auth-user: <id>` | Adds `authUserId: <id>` to the outgoing header set (forwarded as `auth-user-id: <id>` after kebab-casing, REQ-21) and sets the request field `authUserId = <id>`, which the Identity resolver reads (REQ-20). |
| `x-test-add-headers: <JSON object>` | Adds every key/value to the outgoing header set with `addHeaders` (keys are kebab-cased on the way out, REQ-21; later values replace earlier ones, REQ-22). |
| `x-test-echo-route: 1` | Proxy middleware only. Adds the header `x-test-route` with the matched Route as ASCII-only JSON: `{operationId, description, path, routerPath, isBearerAuth, isApiKeyAuth, rateLimits, extraKeys}`, where `operationId` is `null` when absent and `extraKeys` is the sorted key list of `extra`. Scenarios read it at the upstream to see which Route matched. |
| `x-test-middleware: allow` (default) | Allows the request. |
| `x-test-middleware: deny` | Returns false (REQ-18, WS-04). |
| `x-test-middleware: throw` | Fails with an HTTP error: status `x-test-error-status` (default `401`), body `x-test-error-body` parsed as JSON (default `{"message":"test middleware error"}`). A body that is not JSON is passed as a string, which the reference wraps as `{"statusCode":…,"message":"<string>"}` (REQ-19). |
| `x-test-middleware: crash` | Throws a plain error that is not an HTTP error (REQ-24). |

### Static-request predicate

| Header | Effect |
|---|---|
| `x-test-static: 1` | The request is a Static Request (REQ-09). |

### Identity resolver (`throttler.keyResolver: "protocol"`)

Always returns the object form `{ key, authenticated }` (RL-11).

| Header | Effect |
|---|---|
| `x-test-identity-throw: 1` | The resolver throws (REQ-24). Checked first. |
| `x-test-identity-none: 1` | `key` is `null` (RL-12). |
| `x-test-identity: <key>` | `key` is `<key>`. |
| neither, and middleware set `authUserId` | `key` is `user:<authUserId>`. |
| none of the above | `key` is the normalised client address the resolver receives. |
| `x-test-identity-authenticated: true` or `false` | Sets `authenticated`. When absent, `authenticated` is true only when `key` came from `authUserId`. |

### MCP authentication handler

| Header | Effect |
|---|---|
| `x-test-mcp-auth: allow` (default) | Authenticates. |
| `x-test-mcp-auth: deny` | Returns false, so the gateway answers 401 `{"error":"Unauthorized"}` (MCP-04). |
| `x-test-mcp-auth: respond` | Writes its own response, status `x-test-error-status` (default `418`) with body `x-test-error-body` (default `{"message":"test mcp authentication response"}`) as `application/json`, then returns false. |

## Fixtures

Generated by `fixture-generator/` from NestJS controllers using `@nestjs/swagger` and `@hodfords/nestjs-api-gateway-client` 12.0.0 (installed from npm). Regenerate with `pnpm install && pnpm generate` in that directory.

| Fixture | Exercises |
|---|---|
| `users-service.json` | Path parameters; `/v1/users/me` declared after `/v1/users/{id}` (OAS-21); several methods per path including `HEAD`; `x-rate-limits` with and without `status` and stacked on one Route; `x-router-path` with a wildcard, an optional group and an escape; `security` naming `bearer`, the bearer key `auth-user-id` and the API key `x-api-key`; header parameters named like excluded headers (`auth-user-id`, `Permission-In-Any-Guard`); an operation without `operationId`; a JSON body whose `$ref` is used twice, and one with a reference cycle; `x-api-mcp`. |
| `users-service-v2.json` | The same plus `GET /v2/new`, for swapping documents at runtime (OAS-02, OAS-03). |
| `default-service.json` | A second service for the Default Service and Direct Prefix cases (`/oauth/...`, `/health`), and a Route with the same pattern and limit as one in `users-service` (RL-17). |
| `invalid-path-item.json` | A path item with a `parameters` key, which the reference fails to load (OAS-17, Deviation D3). |

Two edits are made after `@nestjs/swagger` runs, because no decorator expresses them: operations marked `x-fixture-drop-operation-id` lose their `operationId`, and `invalid-path-item.json` gets its path-level `parameters`.

## Mock upstream

[`harness/src/mock.rs`](harness/src/mock.rs). Serves a fixture at `/openapi.json` (swappable at runtime, or made to answer an error status or drop the connection), records every other request exactly as received (connection number, method, raw target, every header in order with duplicates and original case, body bytes), and answers according to behaviours chosen by path prefix: echo (default), fixed reply, redirect, delayed reply, large or slow streamed body, close before headers, close mid-body, never answer, WebSocket echo, a WebSocket server that speaks first, and a non-upgrade answer to an upgrade with or without `content-length`.

For manual use: `cargo run -p conformance --bin mock-upstream -- --fixture conformance/fixtures/users-service.json --port 5000`.
