# 05 Rate limiting

Counters live in Redis so that several gateway instances share them. Redis key names and scripts are not part of the External Contract (ADR 0001); the behaviour below is. This file covers Routed Requests; MCP calls are in [08-mcp.md](08-mcp.md).

## Algorithm

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| RL-01 | When rate limiting is disabled, no Bucket is checked or counted anywhere, including the after-response counting of RL-16. | `throttlers/services/throttler.service.ts:79,124,143,196` | C, U | todo |
| RL-02 | Each Bucket is a fixed window. The first request opens the window; `limit` requests are admitted in it; further requests are rejected until the window expires. The window does not slide. | `throttlers/constants/lua-script.constant.ts:6-23` | C | todo |
| RL-03 | A limit of 0 behaves like a limit of 1: the request that opens a window is always admitted. | `throttlers/constants/lua-script.constant.ts:11-22` | C | todo |
| RL-04 | Windows are given in seconds; fractions are allowed and are truncated to whole milliseconds. | `throttlers/services/throttler.service.ts:74-76` | U | todo |
| RL-05 | Check-and-count is atomic, so concurrent requests on several gateway instances cannot exceed the limit. | `throttlers/constants/lua-script.constant.ts:6-23` | R | todo |

## Client address

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| RL-06 | The client address is the one derived from trusted proxies when the application has configured them (Express `trust proxy`, read through `request.ip`), otherwise the connection's peer address. With no address at all, the literal `unknown` is used. The port needs an equivalent trusted-proxy setting, including the presets `loopback`, `linklocal` and `uniquelocal` that the reference sample uses. | `throttlers/services/throttler.service.ts:238-241`; `src/main.ts:23` | C, U | todo |
| RL-07 | The address is normalised: IPv4 is unchanged; IPv4-mapped IPv6 becomes plain IPv4; an IPv6 zone index is dropped; other IPv6 is masked to `ipv6Subnet` bits and written as eight unpadded lowercase hex groups followed by `/<subnet>` (`2001:db8:1:2:0:0:0:0/64`); anything that is not an IP address is unchanged. | `throttlers/helpers/ip.helper.ts:9-71` | U | todo |
| RL-08 | The normalisation function is public, with the subnet defaulting to 64. | `index.ts:15` | U | todo |

## Buckets

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| RL-09 | Global IP Bucket: one per normalised client address, counting every Routed Request, before middleware runs. It is counted at most once per request. | `throttlers/services/throttler.service.ts:142-157`; `restful/services/proxy.service.ts:277` | C, U | todo |
| RL-10 | Anonymous IP Bucket (only when configured): one per normalised client address. It counts a request that middleware rejected (REQ-18, REQ-19) or whose Identity is not authenticated. It is counted at most once per request. | `throttlers/services/throttler.service.ts:98-100,123-140`; `restful/services/proxy.service.ts:283-290` | C, U | todo |
| RL-11 | Identity: with no resolver, it is the normalised client address, not authenticated. A resolver receives the request, the Route and the normalised address, may be asynchronous, and returns a key, nothing, or a key together with an authenticated flag (default false). | `throttlers/services/throttler.service.ts:247-279`; `throttlers/types/throttler-option.type.ts:4-29` | C, U | todo |
| RL-12 | When the resolver returns no key (absent, empty or whitespace only), the Custom Bucket and Route Buckets are skipped for that request. The Global IP Bucket still applies, and so does the Anonymous IP Bucket unless the resolver marked the request authenticated. | `throttlers/services/throttler.service.ts:29-34,102-104,267-269` | C, U | todo |
| RL-13 | The resolver runs after the middleware chain, so it can use what middleware attached to the request (REQ-20). | `restful/services/proxy.service.ts:282,292` | C | todo |
| RL-14 | Custom Bucket: one per Identity, with the configured Custom limit and window, counting every Routed Request of that Identity across all Routes. | `throttlers/services/throttler.service.ts:159-169` | C, U | todo |
| RL-15 | Route Bucket without a status: checked and counted before forwarding, per Identity. | `throttlers/services/throttler.service.ts:171-193` | C, U | todo |
| RL-16 | Route Bucket with a status: before forwarding, the request is rejected only if the Bucket is already full. The Bucket is counted after the response completes, and only when the response status equals the configured status. | `throttlers/services/throttler.service.ts:183-186,195-212`; `restful/services/proxy.service.ts:293-297` | C, U | todo |
| RL-17 | **Deviation D2.** Every entry of a Route's `x-rate-limits` has its own Bucket, distinguished by Upstream Service, method, Route pattern, Identity, limit, window and status. Reference: all entries of a Route share one counter, and so do same-pattern Routes of different services. | `throttlers/services/throttler.service.ts:221-223` | R | todo |
| RL-18 | Order for a Routed Request: Global IP, middleware, Anonymous IP, Custom, then Route Buckets in document order. The first full Bucket rejects the request. Buckets checked earlier have already counted it; later ones do not. | `throttlers/services/throttler.service.ts:78-113`; `restful/services/proxy.service.ts:277-292` | C | todo |
| RL-19 | After-response counting (RL-16) uses the Identity resolved for that request and is skipped when the Identity was "no key". | `throttlers/services/throttler.service.ts:200-204` | U | todo |

## Rejection

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| RL-20 | A full Global IP, Anonymous IP or Custom Bucket answers 429 with body `{"message":"error.too_many_requests"}`. | `throttlers/exceptions/too-many-request.exception.ts:3-7`; `throttlers/services/throttler.service.ts:137-139,154-156,166-168` | C | todo |
| RL-21 | A full Route Bucket answers 429 with body `{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"<text>"}}`. The text comes from the window: under 60 s `less than a minute`; 60–119 s `the next one minute`; up to 59 min `the next N minutes`; 60–119 min `the next one hour`; beyond that `the next N hours`. | `throttlers/services/throttler.service.ts:188-192`; `restful/helpers/string.helper.ts:33-46` | C, U | todo |
| RL-22 | No `Retry-After` or `X-RateLimit-*` header is sent. | `throttlers/exceptions/too-many-request.exception.ts:3-7` | C | todo |
| RL-23 | The window-to-text function is public. | `index.ts:10` | U | todo |

## Scope

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| RL-24 | Static Requests, Bypass Requests, WebSocket upgrades and requests with no matching Route are never rate limited. | `restful/services/proxy.service.ts:224-227,259-273` | C | todo |
