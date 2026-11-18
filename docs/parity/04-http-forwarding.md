# 04 HTTP forwarding

What the Upstream Service receives and what the client gets back. Applies to Routed, Static and Bypass Requests unless a row says otherwise.

## Path and query

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-01 | The Service Prefix is removed only when it is the leading path segment: `/svc` becomes `/`, `/svc/x` becomes `/x`, and `/svcx` is unchanged. | `restful/services/proxy.service.ts:178-188` | C, U | todo |
| FWD-02 | Paths reached through a Direct Prefix or the Default Service are forwarded unchanged. | `restful/services/proxy.service.ts:178-188` | C, U | todo |
| FWD-03 | A trailing `/` is appended to the upstream path when it has none, for every request that is not a Static Request: `/svc/v1/users` is forwarded as `/v1/users/`. | `restful/services/proxy.service.ts:161-168` | C, U | todo |
| FWD-04 | The query string reaches the upstream with the same result as the reference for: a single key, repeated keys, percent-encoded characters, empty values, and a key with no `=`. **Deviation D9:** other shapes may differ; each one found is listed here. | `proxy/proxy-server.ts:79-81` | C | todo |
| FWD-05 | The request method is forwarded unchanged. | `proxy/proxy-server.ts:80` | C | todo |

## Request headers

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-06 | These headers are not forwarded: `connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailers`, `transfer-encoding`, `upgrade`. Headers merely named inside `Connection` are still forwarded. | `proxy/proxy-server.ts:10-19,27-36` | C, U | todo |
| FWD-07 | `x-forwarded-for` is set to the client connection's peer address, or that address is appended after `, ` to the value the client sent. Several incoming `x-forwarded-for` lines are joined with `, ` first. **confirm** the address format for IPv4 clients on the reference's listener. | `proxy/proxy-server.ts:41-50` | C, U | todo |
| FWD-08 | No `x-forwarded-proto`, `x-forwarded-host`, `forwarded` or `via` header is added. | `proxy/proxy-server.ts:38-64` | C | todo |
| FWD-09 | The client's `Host` header is forwarded unchanged; it is not replaced by the upstream's host. **confirm** | `proxy/proxy-server.ts:39,82` | C | todo |
| FWD-10 | `content-length: 0` is added to `DELETE` and `OPTIONS` requests that have no `content-length`. | `proxy/proxy-server.ts:51-53` | C, U | todo |
| FWD-11 | Middleware headers are applied last and replace client headers of the same name. Static and Bypass Requests carry none. | `proxy/proxy-server.ts:54-61`; `restful/services/proxy.service.ts:298-300` | C, U | todo |
| FWD-12 | All other client headers are forwarded as sent, including `authorization`, `cookie` and repeated headers. | `proxy/proxy-server.ts:27-36` | C | todo |

## Bodies and responses

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-13 | The request body is streamed to the upstream without buffering, parsing or a size limit, for any content type. | `proxy/proxy-server.ts:83`; `src/main.ts:16-18` | C | todo |
| FWD-14 | The upstream's status code is returned unchanged. | `proxy/proxy-server.ts:98-99` | C | todo |
| FWD-15 | Upstream response headers are returned unchanged except the FWD-06 list. Repeated headers such as `set-cookie` stay separate. | `proxy/proxy-server.ts:100` | C, U | todo |
| FWD-16 | The `Location` header of a 3xx response is returned unchanged. The reference contains code meant to prefix it with the Service Prefix, but it runs after the headers are already copied. **confirm** before implementing. | `proxy/proxy-server.ts:100-103`; `restful/services/proxy.service.ts:132-141` | C | todo |
| FWD-17 | The response body is streamed to the client as received, without decoding; `content-encoding` is preserved. | `proxy/proxy-server.ts:104` | C | todo |
| FWD-18 | Responses without a body (`HEAD`, 204, 304) are forwarded correctly. | `proxy/proxy-server.ts:97-104` | C | todo |
| FWD-19 | The upstream may be `http` or `https`. | `proxy/proxy-server.ts:24` | R | todo |
| FWD-20 | Connections to each Upstream Service are pooled and reused (CFG-10). | `proxy/proxy-server.ts:22-25` | R | todo |

## Failures

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-21 | When the client disconnects before the response is complete, the upstream request is aborted. | `proxy/proxy-server.ts:77,90-95` | C | todo |
| FWD-22 | When the upstream cannot be reached or fails before sending response headers, the client gets 503 with `Content-Type: application/json` and body `{"message":"Service unavailable."}`. | `restful/services/proxy.service.ts:143-154`; `proxy/proxy-server.ts:105-111` | C | todo |
| FWD-23 | When the upstream fails after response headers were sent, the client's response is cut short and the connection closed. **confirm** what the reference client observes. | `restful/services/proxy.service.ts:143-147`; `proxy/proxy-server.ts:104-111` | C | todo |
