# 06 WebSocket proxying

Upgrade requests take a separate, shorter path than HTTP requests. Several behaviours here differ from HTTP forwarding on purpose, because Upstream Services already depend on them.

## Before forwarding

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| WS-01 | Every HTTP upgrade request, on any path, is handled by the WebSocket path. | `restful/services/proxy.service.ts:53-62` | C | todo |
| WS-02 | The URL is normalised and `excludeHeaders` are removed, as in REQ-02 and REQ-03. | `restful/services/proxy.service.ts:237` | C | todo |
| WS-03 | The Upstream Service is selected with the rules of REQ-04 to REQ-07. | `restful/services/proxy.service.ts:238` | C | todo |
| WS-04 | WebSocket middleware run in ascending priority after `excludeHeaders` are removed again; each receives the request and the outgoing header set. The first one that does not allow the request stops the chain. There is no Route lookup, no rate limiting, and no Static or Bypass handling. | `restful/services/ws-request.service.ts:25-47`; `restful/services/proxy.service.ts:239-242` | C, U | todo |
| WS-05 | **Deviation D1.** A rejected upgrade is answered and the connection closed: 403 when middleware disallows it, the middleware's own status when it fails with one, 503 when no Upstream Service matches. The body is the same JSON as the HTTP path. Reference: the error is only logged, and the client connection stays open with no answer. | `restful/services/proxy.service.ts:55-62,240-242` | R | todo |
| WS-06 | A request that is not `GET`, or whose `Upgrade` header is not `websocket` (any letter case), has its connection dropped with no response. This check runs after the middleware chain. | `proxy/proxy-socket.ts:17-23,72-74` | C, U | todo |

## Upstream request

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| WS-07 | The upstream path is the full normalised URL including the Service Prefix and query. The prefix is not removed and no trailing slash is added. | `proxy/proxy-socket.ts:60` | C, U | todo |
| WS-08 | All client headers are forwarded, including `connection`, `upgrade` and `sec-websocket-*`, as Node holds them: names lower-cased, a repeated header joined into one line with `, ` (`; ` for `cookie`), a header Node keeps once (such as `user-agent`) with its first value only, `set-cookie` once per value. Middleware headers replace same-named ones in place or are appended. No `x-forwarded-for` is added and `Host` is unchanged. | `proxy/proxy-socket.ts:56-59` | C, U | todo |
| WS-09 | The upstream connection is plain HTTP even when the service host is `https`, and is a new connection for every upgrade, never one from the pool. | `proxy/proxy-socket.ts:48-69,76` | C, U | todo |

## Tunnel

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| WS-10 | When the upstream accepts, the client receives `HTTP/1.1 101 Switching Protocols` (whatever reason the upstream sent) followed by the upstream's response headers, names lower-cased. A header the upstream repeated is written as one line with the values joined by `, ` (Node joins them before the gateway sees them), except a header Node keeps once (such as `content-type`), which keeps its first value; only `set-cookie` is written once per value. Observed by the HTTP stack spike's checks against the reference. | `proxy/proxy-socket.ts:25-46,119` | C, U | todo |
| WS-11 | After the 101, bytes flow both ways unmodified. | `proxy/proxy-socket.ts:121` | C | todo |
| WS-12 | Bytes that arrive together with the handshake are not lost in either direction. A server that speaks first (Engine.IO sends its open packet immediately) is received intact. | `proxy/proxy-socket.ts:100-117` | C, U | todo |
| WS-13 | The tunnel has no idle timeout on either side; an idle connection stays open. | `proxy/proxy-socket.ts:77-79,109` | C, U | todo |
| WS-14 | When either side closes, the other side is closed. | `proxy/proxy-socket.ts:96-98,121` | C | todo |

## Upstream refuses or fails

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| WS-15 | When the upstream answers without upgrading (for example 401), the client receives a status line with the upstream's status and reason, the upstream's body, and then the connection is closed. The upstream's response headers are not passed on. If the client spoke HTTP/1.1 and the upstream sent no `content-length`, a single `transfer-encoding: chunked` header is sent and the body is chunk-framed; otherwise no header is sent at all. | `proxy/proxy-socket.ts:124-156` | C, U | todo |
| WS-16 | When the upstream cannot be reached, the client connection is closed with no response. | `proxy/proxy-socket.ts:81-85` | C | todo |
