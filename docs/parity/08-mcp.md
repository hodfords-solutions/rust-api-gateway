# 08 MCP server

Present only when `mcp.enabled` is true (CFG-13). Built behind the cargo feature `mcp`. Each Route can be exposed as an MCP tool that calls the Upstream Service.

Rows MCP-15 and MCP-16 keep reference quirks on purpose (approved under Q17 row 11).

## Endpoint

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| MCP-01 | The endpoint is `POST /mcp` only. Other methods on `/mcp` go to the proxy. | `mcp/controllers/mcp.controller.ts:9-12` | C | todo |
| MCP-02 | Streamable HTTP transport in stateless mode: every request is handled on its own, with no session id. `initialize`, `tools/list`, `tools/call` and notifications behave as the reference does. Compared at the JSON-RPC message level. | `mcp/services/mcp-server.service.ts:34-42` | C | todo |
| MCP-03 | Server info defaults to name `nestjs-api-gateway` and version `1.0.0`, each replaceable by options. The server advertises the capability `tools.listChanged`. | `mcp/services/mcp-server.service.ts:57-63` | C | todo |
| MCP-04 | MCP authentication handlers run in ascending priority before any protocol handling. The first that does not authenticate ends the request with 401 and body `{"error":"Unauthorized"}`, unless the handler already wrote its own response. | `mcp/services/mcp-server.service.ts:44-54` | C | todo |

## Tool registry

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| MCP-05 | The tool list is rebuilt after the first document load has settled and then every 5 minutes. A Route added upstream therefore appears as a tool within 5 minutes, not within the 10 seconds of OAS-02. | `mcp/services/mcp-tool-registry.service.ts:23-29,39-88`; `mcp/services/mcp-server.service.ts:28-32` | R | todo |
| MCP-06 | One tool is built per Route of every Upstream Service with a loaded document, subject to three filters: `allowedServices`; `allowedOperations` (a Route with no operation id passes it); and the `filter` callback, which receives the service prefix and the Route. | `mcp/services/mcp-tool-registry.service.ts:43-64` | C, U | todo |
| MCP-07 | Tool name: `<service>_<operationId>`, or without an operation id `<service>_<METHOD>_<path>` where `{`, `}` and `/` in the path become `_`, runs of `_` collapse and leading or trailing `_` are trimmed. A `toolName` callback can override it and receives the service, method, operation id, path and the default name. | `mcp/services/mcp-tool-registry.service.ts:90-115` | C, U | todo |
| MCP-08 | A name longer than 64 characters is logged as a warning and used as is. Duplicate names are kept; a call resolves to the first. | `mcp/services/mcp-tool-registry.service.ts:35-37,96-101`; `mcp/constants/mcp.constant.ts:7` | U | todo |
| MCP-09 | Tool description: the Route's description, or `<METHOD> <path>` when it is empty. | `mcp/services/mcp-tool-registry.service.ts:75` | U | todo |
| MCP-10 | Input schema: an object. Each operation parameter becomes the property `<in>_<name>` with a type (default `string`), a description (default `<in> parameter: <name>`), its `enum` if any, and membership in `required` when the parameter is required. A `parameterFilter` callback can drop a parameter. `required` is omitted when empty. | `mcp/services/mcp-tool-registry.service.ts:117-183` | C, U | todo |
| MCP-11 | Types are normalised to JSON Schema: valid names are lower-cased; a list of types is normalised entry by entry; a stringified constructor such as `function String() { [native code] }` maps `String`→`string`, `Number` and `BigInt`→`number`, `Boolean`→`boolean`, `Array`→`array`, `Object`→`object`, `Date`→`string`. Anything else is dropped from a schema and becomes `string` for a parameter. | `mcp/services/mcp-tool-registry.service.ts:229-312` | U | todo |
| MCP-12 | An `application/json` request body becomes the property `body`: `{ "type": "object", "description": <body description or "Request body">, ...resolved schema }`, required when the body is required. Local `$ref`s are resolved recursively through `properties` and `items`; a reference cycle resolves to `{}`. `allOf`, `oneOf` and `anyOf` are passed through unresolved. **Deviation D8:** a schema referenced from several places resolves each time. Reference: only the first resolves; the rest become `{}`. | `mcp/services/mcp-tool-registry.service.ts:134-147,185-219` | C, U, R | todo |
| MCP-13 | `tools/list` returns each tool's name, description and input schema. | `mcp/services/mcp-server.service.ts:84-93` | C | todo |

## Tool calls

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| MCP-14 | Calling an unknown tool returns an error result with text `Tool not found: <name>`. | `mcp/services/mcp-tool-executor.service.ts:37-43` | C, U | todo |
| MCP-15 | A call first runs rate limiting against the `/mcp` request, in the order Global IP, Identity, Anonymous IP, Custom, Route Buckets, and only then the proxy middleware chain with the tool's Route. Consequences kept from the reference: the Identity resolver runs before middleware; the Route Bucket is keyed with method `POST`; status-scoped Route Buckets are checked but never counted. | `mcp/services/mcp-tool-executor.service.ts:45-62`; `throttlers/services/throttler.service.ts:78-113` | C, U | todo |
| MCP-16 | The tool arguments are exposed to middleware as the request body. `excludeHeaders` are removed from the `/mcp` request before middleware runs. Middleware that disallows the call produces an error result with text `Request forbidden by gateway middleware`. | `mcp/services/mcp-tool-executor.service.ts:46-62`; `restful/services/request.service.ts:48-53` | C, U | todo |
| MCP-17 | The upstream request is built from the arguments: `path_<x>` replaces `{x}` in the OpenAPI path (**Deviation D7:** URL-encoded; reference: raw); `query_<x>` become the query string with form encoding; `header_<x>` become headers except `excludeHeaders` names; `body` is sent as JSON for `POST`, `PUT` and `PATCH` only. A trailing `/` is appended to the path. Values are converted to strings. | `mcp/services/mcp-tool-executor.service.ts:94-183` | C, U, R | todo |
| MCP-18 | Upstream headers are, in increasing precedence: `content-type: application/json`, the `header_<x>` arguments, then middleware headers. Empty values are dropped. Headers of the `/mcp` request itself are not forwarded. | `mcp/services/mcp-tool-executor.service.ts:99-113` | C, U | todo |
| MCP-19 | The result is the upstream response body as UTF-8 text. An upstream status of 400 or above gives an error result `Error executing <tool>: Downstream service returned <status>: <body>`. A rate-limit rejection gives `Error executing <tool>: error.too_many_requests` (Global IP and Custom Buckets) or `Error executing <tool>: error.too_many_requests_with_custom_ttl` (Route Bucket; the TTL text is not included). Any other failure gives `Error executing <tool>: <message>` with the HTTP client's message (undici: `other side closed`). | `mcp/services/mcp-tool-executor.service.ts:64-75,135-147` | C (`mcp::mcp_19_tool_results`), U | todo |
| MCP-20 | Tool calls use a connection pool per Upstream Service with the CFG-10 options, and the Service Prefix is not part of the upstream path. | `mcp/services/mcp-tool-executor.service.ts:27-29,83-91` | R | todo |
