# 04 HTTP forwarding

What the Upstream Service receives and what the client gets back. Applies to Routed, Static and Bypass Requests unless a row says otherwise.

## Path and query

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-01 | The Service Prefix is removed only when it is the leading path segment: `/svc` becomes `/`, `/svc/x` becomes `/x`, and `/svcx` is unchanged. | `restful/services/proxy.service.ts:178-188` | C, U | todo |
| FWD-02 | Paths reached through a Direct Prefix or the Default Service are forwarded unchanged. | `restful/services/proxy.service.ts:178-188` | C, U | todo |
| FWD-03 | A trailing `/` is appended to the upstream path when it has none, for every request that is not a Static Request: `/svc/v1/users` is forwarded as `/v1/users/`. | `restful/services/proxy.service.ts:161-168` | C, U | todo |
| FWD-04 | The query string is parsed (Express 5 simple parser, `node:querystring`) and re-serialised (undici), so the upstream receives a normalised form. Unchanged: a single key, repeated keys in a row, percent-encoded characters, empty values (`a=`). Normalised: values of a repeated key are grouped at the key's first position (`a=1&b=2&a=3` → `a=1&a=3&b=2`); a key without `=` gets one (`a` → `a=`); empty pairs and a bare `?` are dropped; values are decoded and re-encoded with `querystring.escape` (`%41` → `A`, `+` → `%20`, `%zz` → `%25zz`, `[` `]` `;` `"` `<` `>` `\` and a second `=` are encoded, `'~!*()` stay literal). A raw non-ASCII byte in the request target is rejected by Node with 400. The full table is in the test. **Deviation D9:** shapes beyond the unchanged ones may differ; each one found is listed here. | `proxy/proxy-server.ts:79-81` | C (`forwarding::fwd_04_query_strings`) | todo |
| FWD-05 | The request method is forwarded unchanged. | `proxy/proxy-server.ts:80` | C | todo |

## Request headers

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-06 | These headers are not forwarded: `connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailers`, `transfer-encoding`, `upgrade`. Headers merely named inside `Connection` are still forwarded. | `proxy/proxy-server.ts:10-19,27-36` | C, U | todo |
| FWD-07 | `x-forwarded-for` is set to the client connection's peer address as Node reports it on its dual-stack listener: `::ffff:127.0.0.1` for an IPv4 client, `::1` for an IPv6 client. When the client sent the header, that address is appended after `, `; several incoming lines are joined with `, ` first; an empty value counts as absent. | `proxy/proxy-server.ts:41-50` | C (`forwarding::fwd_07_x_forwarded_for`), U | todo |
| FWD-08 | No `x-forwarded-proto`, `x-forwarded-host`, `forwarded` or `via` header is added. | `proxy/proxy-server.ts:38-64` | C | todo |
| FWD-09 | The client's `Host` header is forwarded unchanged, including letter case and port; it is not replaced by the upstream's host. | `proxy/proxy-server.ts:39,82` | C (`forwarding::fwd_09_host_header`) | todo |
| FWD-10 | `content-length: 0` is added to `DELETE` and `OPTIONS` requests that have no `content-length`. | `proxy/proxy-server.ts:51-53` | C, U | todo |
| FWD-11 | Middleware headers are applied last and replace client headers of the same name. Static and Bypass Requests carry none. | `proxy/proxy-server.ts:54-61`; `restful/services/proxy.service.ts:298-300` | C, U | todo |
| FWD-12 | All other client headers are forwarded as sent, including `authorization`, `cookie` and repeated headers. | `proxy/proxy-server.ts:27-36` | C | todo |

## Bodies and responses

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-13 | The request body is streamed to the upstream without buffering, parsing or a size limit, for any content type. | `proxy/proxy-server.ts:83`; `src/main.ts:16-18` | C | todo |
| FWD-14 | The upstream's status code is returned unchanged. | `proxy/proxy-server.ts:98-99` | C | todo |
| FWD-15 | Upstream response headers are returned unchanged except the FWD-06 list. Repeated headers such as `set-cookie` stay separate. | `proxy/proxy-server.ts:100` | C, U | todo |
| FWD-16 | The `Location` header of a 3xx response is returned unchanged, whether absolute, root-relative or relative. The reference's code meant to prefix it with the Service Prefix runs after the headers are copied and has no effect. | `proxy/proxy-server.ts:100-103`; `restful/services/proxy.service.ts:132-141` | C (`forwarding::fwd_16_location_unchanged`) | todo |
| FWD-17 | The response body is streamed to the client as received, without decoding; `content-encoding` is preserved. | `proxy/proxy-server.ts:104` | C | todo |
| FWD-18 | Responses without a body (`HEAD`, 204, 304) are forwarded correctly. | `proxy/proxy-server.ts:97-104` | C | todo |
| FWD-19 | The upstream may be `http` or `https`. | `proxy/proxy-server.ts:24` | R | todo |
| FWD-20 | Connections to each Upstream Service are pooled and reused (CFG-10). | `proxy/proxy-server.ts:22-25` | R | todo |

## Failures

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| FWD-21 | When the client disconnects before the response is complete, the upstream request is aborted. | `proxy/proxy-server.ts:77,90-95` | C | todo |
| FWD-22 | When the upstream cannot be reached or fails before sending response headers, the client gets 503 with `Content-Type: application/json` and body `{"message":"Service unavailable."}`. | `restful/services/proxy.service.ts:143-154`; `proxy/proxy-server.ts:105-111` | C | todo |
| FWD-23 | When the upstream fails after response headers were sent, the client receives the status, the headers and the body bytes received so far, then the connection is closed with no error body: with an upstream `content-length` the client sees that length and a short body; with a chunked upstream the chunked response lacks its final chunk. Other requests are unaffected. | `restful/services/proxy.service.ts:143-147`; `proxy/proxy-server.ts:104-111` | C (`forwarding::fwd_23_upstream_fails_mid_body`) | todo |
