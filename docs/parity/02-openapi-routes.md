# 02 OpenAPI ingestion and the Route table

The gateway has no route configuration of its own. It reads each Upstream Service's OpenAPI document and builds Routes from it.

## Loading

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| OAS-01 | At startup the gateway fetches every Upstream Service's document URL with a plain `GET`, all services concurrently. A failed or non-200 fetch leaves that service without a document and is logged; startup continues. | `restful/services/proxy.service.ts:64-83`; `restful/services/open-api.service.ts:47-53` | C | todo |
| OAS-02 | Every 10 seconds the gateway re-fetches every service's document. A change in the document changes routing without a restart. | `restful/tasks/update-api-document.task.ts:19-26` | C | todo |
| OAS-03 | A failed refresh keeps the previously loaded document. A service that once loaded is never unloaded. | `restful/tasks/update-api-document.task.ts:22-24` | C | todo |
| OAS-04 | A document identical to the last loaded one is not rebuilt. | `restful/services/open-api.service.ts:55-59` | U | todo |
| OAS-05 | A service "has a document" from its first successful load onward. Service selection depends on this (REQ-07). | `restful/services/open-api.service.ts:61-62` | U | todo |

## Building Routes

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| OAS-06 | Routes are grouped by method. Eight methods are supported: `get`, `post`, `delete`, `patch`, `put`, `options`, `head`, `search`. | `restful/services/open-api.service.ts:76-88` | U | todo |
| OAS-07 | Each operation under `paths` becomes one Route, kept in document order within its method. | `restful/services/open-api.service.ts:89-108` | U | todo |
| OAS-08 | A Route carries: operation id, description (empty string when absent), the OpenAPI path template, the match pattern, rate limits, a bearer flag, an API-key flag, and `extra`. | `restful/services/open-api.service.ts:97-107`; `restful/types/router-path.type.ts:7-17` | U | todo |
| OAS-09 | Operation keys are camel-cased one level deep before they are read, so `x-rate-limits` is read as `xRateLimits` and `x-router-path` as `xRouterPath`. | `restful/services/open-api.service.ts:92` | U | todo |
| OAS-10 | The match pattern is `x-router-path` when the operation has one. Otherwise it is the OpenAPI path with each `{name}` turned into `:name`, for names made of word characters. | `restful/services/open-api.service.ts:93,103,164-167` | U | todo |
| OAS-11 | Extension Contract: `x-rate-limits` is a list of `{ limit, ttl, status? }`. Absent means no Route limits. | `restful/services/open-api.service.ts:105`; `libs/client/decorators/api-rate-limit.decorator.ts:3-7` | C | todo |
| OAS-12 | The bearer flag is set when any security requirement of the operation names `bearer` or one of the configured bearer keys. | `restful/services/open-api.service.ts:129-144` | U | todo |
| OAS-13 | The API-key flag is set when any security requirement names one of the configured API-key names. It is always false when none are configured. | `restful/services/open-api.service.ts:146-162` | U | todo |
| OAS-14 | `extra` holds every other camel-cased operation key except `parameters`, `responses`, `xRateLimits` and `xRouterPath`. It includes `security`, `tags`, `summary`, `requestBody` and unknown extensions. | `restful/services/open-api.service.ts:118-127` | U | todo |
| OAS-15 | Extension Contract: `x-api-mcp` (a list of `{ description? }`) is not interpreted by the gateway. It is only visible as `extra.xApiMcp`, for an application's MCP filter to use. | `libs/client/decorators/api-mcp.decorator.ts:3-7`; `restful/services/open-api.service.ts:118-127` | U | todo |
| OAS-16 | The flags are information for middleware. The gateway itself never rejects a request because a Route requires authentication. | `restful/services/open-api.service.ts:101-102` | C | todo |
| OAS-17 | **Deviation D3.** A path-item key that is not one of the eight methods (`parameters`, `summary`, `description`, `servers`, `$ref`, `trace`) is skipped. Reference: building the table throws, and the document does not load. | `restful/services/open-api.service.ts:91-97` | R | todo |

## Matching

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| OAS-18 | Matching uses the path only; the query string is ignored. | `restful/services/open-api.service.ts:67`; `restful/helpers/string.helper.ts:20-26` | U | todo |
| OAS-19 | Matching follows `path-to-regexp` 8 `match()`: the whole path must match, letter case is ignored, one trailing slash is optional (two are not), and a `:name` parameter matches exactly one non-empty segment, which may contain `%2F`, `;` or percent-encoded UTF-8. The forwarded path keeps the client's letter case. Two inputs never reach matching: malformed percent-encoding in the path (`/svc/v1/users/%zz`) is rejected by Express's router with 400 `{"message":"Failed to decode param '%zz'","error":"Bad Request","statusCode":400}`, and a lower-case method is rejected by Node's parser with 400. | `restful/services/open-api.service.ts:93-95` | C (`routes::oas_19_route_matching`), U | todo |
| OAS-20 | `x-router-path` is compiled by `path-to-regexp` 8, so its full syntax works: `*name` matches one or more segments (`/files/*path` matches `/files/a/b/c`, not `/files/` or `/files`); `{/:id}` makes a group optional; a backslash escapes a literal (`escaped\:literal` matches `escaped:literal`, not `escaped%3Aliteral`). Which forms real upstream services emit is still unknown; the test only proves what the reference accepts. | `restful/services/open-api.service.ts:93` | C (`routes::oas_20_router_path_syntax`), U | todo |
| OAS-21 | Among Routes of the requested method, the first match in document order wins. There is no preference for literal paths over parameters. | `restful/services/open-api.service.ts:65-73` | C | todo |
| OAS-22 | The method is compared case-insensitively. | `restful/services/open-api.service.ts:66` | U | todo |

## For review

These are crash paths in the reference that the approved Deviations do not name. Proposed handling is stated; say if you want otherwise.

- **Document without `info`.** Reference: building the table throws on `doc.info.title`, so the document does not load. Proposed: load it anyway, as a natural extension of D3, since title and version are never used for routing.
- **Malformed percent-encoding in a request path** (for example `/svc/users/%zz`). Reference: parameter decoding throws, giving 500. Proposed: treat as no match (405).
