# 03 Request pipeline

How an HTTP request is classified and what runs before it is forwarded. Forwarding itself is in [04-http-forwarding.md](04-http-forwarding.md); the rate-limit steps are detailed in [05-rate-limiting.md](05-rate-limiting.md).

## Entry and sanitising

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| REQ-01 | Every request, of any method and path, that no application route or gateway endpoint (`/documents`, `/document-json`, `POST /mcp`) claims is handled by the proxy. | `restful/controllers/proxy.controller.ts:9-12` | C | todo |
| REQ-02 | Before any decision, the URL is normalised the way the WHATWG URL parser does: `.` and `..` segments (also as `%2e`) are resolved, backslashes become slashes, the fragment is dropped, and tab, CR and LF are removed. A repeated slash inside the path is kept; `..%2f` and `;` are left alone. Routing and the forwarded path both use the normalised URL. | `restful/helpers/request-sanitizer.helper.ts:15-20`; `restful/services/proxy.service.ts:199-202` | C, U | todo |
| REQ-03 | Before any decision, every `excludeHeaders` name is removed from the incoming request, matched case-insensitively. This applies to Routed, Static and Bypass Requests and to WebSocket upgrades. | `restful/helpers/request-sanitizer.helper.ts:31-38`; `restful/services/proxy.service.ts:199-202` | C, U | todo |

## Selecting the Upstream Service

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| REQ-04 | Direct Prefixes are tried first, in configuration order. When two services declare the same Direct Prefix, the first configured service owns it. | `restful/services/proxy.service.ts:96-103,312-316` | C, U | todo |
| REQ-05 | Service Prefixes are tried next, in configuration order; the first match wins. | `restful/services/proxy.service.ts:317-323` | C, U | todo |
| REQ-06 | A prefix matches when the URL equals `prefix` or `/prefix`, or starts with one of them followed by `/`, `?` or `#`. Matching is case-sensitive and whole-segment: `oauthtoken` does not match the prefix `oauth`. | `restful/helpers/prefix.helper.ts:12-25` | U | todo |
| REQ-07 | When no prefix matches, or the matched Service Prefix belongs to a service with no loaded document, the request goes to the Default Service (the one whose prefix is `default`) if configured. Otherwise the gateway answers 503. | `restful/services/proxy.service.ts:324-329` | C, U | todo |
| REQ-08 | A Direct Prefix match does not require the owning service's document to be loaded: Static and Bypass Requests are still forwarded. **Deviation D4:** a Routed Request to a Direct Prefix owner or Default Service with no loaded document answers 503. Reference: 500. | `restful/services/proxy.service.ts:312-316`; `restful/services/open-api.service.ts:66` | C, R | todo |

## Request classes

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| REQ-09 | A request is a Static Request when any registered static predicate returns true for it. This is checked first, before bypass prefixes. | `restful/services/request.service.ts:60-67`; `restful/services/proxy.service.ts:209-216` | C, U | todo |
| REQ-10 | A Static Request is forwarded with no Route lookup, no middleware, no rate limiting and no middleware headers. Its Service Prefix is stripped; no trailing slash is added. | `restful/services/proxy.service.ts:224-227,164` | C, U | todo |
| REQ-11 | Service selection runs for every class. A Static or Bypass Request with no matching service and no Default Service answers 503. | `restful/services/proxy.service.ts:225,258` | C | todo |
| REQ-12 | A request is a Bypass Request when its normalised URL matches any `bypassRoutePrefixes` entry under the REQ-06 rule. The entry is compared with the full URL, so it includes the Service Prefix when there is one. | `restful/services/proxy.service.ts:259-263` | C, U | todo |
| REQ-13 | A Bypass Request is forwarded with no Route lookup, no middleware, no rate limiting and no middleware headers. Its Service Prefix is stripped and a trailing slash is added (FWD-03). | `restful/services/proxy.service.ts:260-263` | C | todo |

## Routed Requests

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| REQ-14 | The Route is looked up with the request method and the URL after removing the Service Prefix. No matching Route answers 405, never 404. | `restful/services/proxy.service.ts:265-273` | C, U | todo |
| REQ-15 | **Deviation D4.** A method outside the eight supported ones answers 405. Reference: 500. | `restful/services/open-api.service.ts:66-68` | R | todo |
| REQ-16 | Steps run in this order: Route lookup, Global IP Bucket, middleware chain, remaining rate limits, forward. A request with no Route is therefore not rate limited, and a request rejected by middleware has already counted against the Global IP Bucket. | `restful/services/proxy.service.ts:257-301` | C | todo |
| REQ-17 | Before the middleware chain, `excludeHeaders` are removed from the request again and from the outgoing header set. Then proxy middleware run in ascending priority; each receives the Route, the request and the outgoing header set. The first one that does not allow the request stops the chain. | `restful/services/request.service.ts:30-53` | C, U | todo |
| REQ-18 | A middleware that disallows the request causes a 403 response. The Anonymous IP Bucket is counted first (RL-10). | `restful/services/proxy.service.ts:288-291` | C | todo |
| REQ-19 | A middleware may fail with its own status and body (for example 401). That response is returned to the client. The Anonymous IP Bucket is counted first, and if it is full the client gets 429 instead. | `restful/services/proxy.service.ts:281-287` | C | todo |
| REQ-20 | Middleware can attach values to the request that later middleware and the Identity resolver can read (the reference sets fields such as `authUserId` on the request object). | `src/middleware/authentication.middleware.ts:6-9`; `src/app.module.ts:27-34` | R | todo |
| REQ-21 | Header names added by middleware are converted to kebab-case with the rules of es-toolkit `kebabCase`: `authUserId` becomes `auth-user-id`, and `oauth2Token` becomes `oauth-2-token`. | `restful/helpers/object.helper.ts:10-33`; `restful/models/proxy-request.model.ts:14-16` | U | todo |
| REQ-22 | The outgoing header set supports adding one header and merging several; a later value for the same name replaces the earlier one. | `restful/models/proxy-request.model.ts:6-12` | U | todo |

## Errors

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| REQ-23 | Gateway errors use the JSON bodies of NestJS's default exception filter: 403, 405, 503 (no service), 429 (RL-20, RL-21) and 500. **confirm** each exact body by Conformance Test before implementing. | `restful/services/proxy.service.ts:272,290,328` | C | todo |
| REQ-24 | An unexpected failure inside the pipeline (Identity resolver error, Redis unreachable) answers 500 with the default body. **confirm** | `throttlers/services/throttler.service.ts:74-76,253` | C | todo |
| REQ-25 | Rust addition (no reference counterpart): the application can replace the default error bodies with its own mapping. The reference achieves this with a NestJS exception filter in the host application. | none | R | todo |
