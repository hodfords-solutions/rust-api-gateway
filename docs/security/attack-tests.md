# Security attack tests

Adversarial integration tests of `api-gateway`, run against the gateway served in-process
(`crates/rust-gateway/tests/security_*.rs`). Most send raw bytes over TCP, because an HTTP
client would resolve or reject the attack before sending it. Each test checks what the upstream
received (`Seen`) and what the client got back.

They run with the rest of the suite (`cargo test --workspace`). `security_rate_limit.rs` needs
Redis and follows the usual convention: `SKIP` without it, a failure under
`RATE_LIMIT_REQUIRE_REDIS=1` (see [development.md](../development.md)).

## How findings are handled

The gateway's behaviour is pinned to the Reference Gateway by the Parity Checklist, so a
weakness is **not fixed here**. Its test asserts the safe behaviour and is marked
`#[ignore = "security finding SEC-NN: …"]`; each fix must be approved individually as a
Deviation (`docs/parity/README.md`), and the fix un-ignores the test. Run the findings with:

```bash
cargo test -p api-gateway --no-fail-fast --test 'security_*' -- --ignored
```

"Reference" below says whether `@hodfords/nestjs-api-gateway` 12.4.2 (the version the Parity
Checklist pins) likely behaves the same, judged from the Parity Checklist rows and Node's
defaults; the reference was not run for these tests.

## Findings

| ID | Severity | Area | Summary | Reference |
|---|---|---|---|---|
| SEC-01 | High | WebSocket | Upgrade requests bypass Route matching, proxy middleware and rate limiting | Same |
| SEC-02 | Medium | Limits | No timeout for a slow request head or body (slowloris) | Differs: Node bounds both |
| SEC-03 | Medium | Limits | Idle keep-alive connections are never closed | Differs: Node closes after 5 s |
| SEC-04 | Medium | Paths | Encoded traversal under a Bypass prefix or Static Request is forwarded raw | Same |
| SEC-05 | Medium | Headers | The underscore spelling of an excluded identity header is forwarded | Likely same |
| SEC-06 | Low | Headers | Spoofed `X-Real-IP`, `X-Forwarded-Host`, `X-Forwarded-Proto`, `Forwarded` reach the upstream | Same |
| SEC-07 | Low | Headers | Headers nominated by `Connection` are forwarded | Same |
| SEC-08 | Low | Rate limiting | Authenticated and anonymous Identities with the same key share Buckets | Likely same |

### SEC-01: upgrade requests bypass Route matching, proxy middleware and rate limiting

- **Tests:** `security_websocket::sec_01_upgrade_requests_cannot_bypass_the_proxy_middleware`,
  `security_websocket::sec_01_upgrades_on_unrouted_paths_are_refused`,
  `security_rate_limit::sec_01_upgrade_requests_are_rate_limited`.
- **Reproduction:** with a proxy middleware that rejects every request (the application's
  authentication), `GET /v1/users` to the Default Service answers 403. The same request with
  `Connection: Upgrade` and `Upgrade: websocket` added is forwarded to the upstream; an upstream
  that ignores the upgrade answers 200, and the gateway relays `HTTP/1.1 200 OK` and the body.
  An upgrade on `/svc/internal/admin` (no Route) is tunnelled (`101`); with the Global IP Bucket
  full, upgrade requests still reach the upstream.
- **Observed:** every upgrade request takes the WebSocket path (WS-01), which has no Route
  lookup, no proxy middleware and no rate limiting (WS-04); only WebSocket middleware run. A
  non-101 upstream answer is relayed (WS-15).
- **Safe:** upgrades are subject to the same authentication and rate limiting as plain
  requests (or refused when they are not WebSocket Routes), and nothing is forwarded otherwise.
- **Severity: high.** An application that enforces authentication in a proxy middleware and
  registers no equivalent WebSocket middleware exposes every `GET` endpoint reachable at the
  forwarded path, without authentication or rate limits, to any client that adds two headers.
  The forwarded path keeps the Service Prefix (WS-07), so endpoints reached through the Default
  Service or a Direct Prefix are exposed directly; whether the upstream answers a non-WebSocket
  upgrade normally depends on its framework (most do).
- **Reference:** same (WS-01, WS-04, WS-07, WS-15 pin this behaviour).

### SEC-02: no timeout for a slow request head or body

- **Tests:** `security_limits::sec_02_slow_request_heads_time_out`,
  `security_limits::sec_02_slow_request_bodies_time_out`.
- **Reproduction:** open a connection, send `GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n`, then
  one byte every 300 ms; or send a `POST` head with `Content-Length: 100000` and trickle the
  body.
- **Observed:** the connection stays open as long as the client keeps sending. hyper's server
  connection is built without a timer, so no header read timeout applies, and nothing bounds a
  whole request. A head ending lines with bare LF is also held unanswered until the client
  leaves (`security_headers::bare_lf_line_endings_are_not_forwarded`), because the gateway's
  framing tracker waits for `\r\n\r\n`.
- **Safe:** a head (and a whole request) that does not complete within a bounded time is
  answered with 408 or closed. The tests use a 5 s bound (`BOUND` in `security_limits.rs`) as a
  placeholder for the timeout the Deviation chooses.
- **Severity: medium.** A few clients holding many slow connections exhaust the gateway's
  connections and file descriptors (slowloris); a slow body also holds an upstream connection.
- **Reference:** differs. Node bounds a request head with `headersTimeout` (60 s) and a whole
  request with `requestTimeout` (300 s); the reference app sets neither, so the defaults apply.

### SEC-03: idle keep-alive connections are never closed

- **Test:** `security_limits::sec_03_idle_keep_alive_connections_are_closed`.
- **Reproduction:** send one request on a keep-alive connection, read the response (it carries
  `Keep-Alive: timeout=5`), then send nothing.
- **Observed:** the connection is still open after the announced 5 s (and indefinitely).
- **Safe:** the connection is closed after the announced timeout.
- **Severity: medium.** Idle connections accumulate without limit; unlike SEC-02 the client
  need not send anything after its first request.
- **Reference:** differs. Node closes an idle connection after `keepAliveTimeout` (5 s), the
  value the gateway announces.

### SEC-04: encoded traversal under a Bypass prefix or Static Request is forwarded raw

- **Test:** `security_paths::sec_04_encoded_traversal_cannot_escape_a_bypass_prefix_or_static_space`.
- **Reproduction:** with the Bypass prefix `svc/public`, `GET /svc/public/..%2f..%2fadmin`
  is forwarded as `/public/..%2f..%2fadmin/`; `GET /svc/public/x/..;/..;/admin` as
  `/public/x/..;/..;/admin/`. The same holds for Static Requests.
- **Observed:** URL normalisation resolves `..`, `%2e%2e` and backslashes (REQ-02), but leaves
  `..%2f`, `..%5c` and `;` path parameters alone. Bypass and Static Requests have no Route
  lookup, so nothing stops them.
- **Safe:** no forwarded target holds a `..` segment hidden behind an encoded separator or a
  `;` parameter (reject such requests, or forward a path that cannot resolve above the prefix).
- **Severity: medium.** An upstream that decodes `%2F`/`%5C` before routing, or treats `;` as
  a path-parameter delimiter (Tomcat and Spring), resolves the path to `/admin`, reached
  without the middleware that guards Routed Requests. Node/Express upstreams are not affected.
  Routed Requests are safe: such paths match no Route and are answered 405.
- **Reference:** same (REQ-02: "`..%2f` and `;` are left alone").

### SEC-05: the underscore spelling of an excluded identity header is forwarded

- **Test:** `security_headers::sec_05_underscore_spellings_of_excluded_headers_are_not_forwarded`.
- **Reproduction:** with `excludeHeaders: ["auth-user-id"]`, send `auth_user_id: admin` (or
  `Auth_User-Id: admin`) to a Routed, Bypass or Static path.
- **Observed:** the header reaches the upstream unchanged; only the exact name (in any letter
  case) is removed (REQ-03).
- **Safe:** names that differ from an excluded name only by `_` for `-` are removed too (or the
  request is refused).
- **Severity: medium.** Upstreams that map `-` and `_` to the same variable (CGI/WSGI and PHP
  `HTTP_AUTH_USER_ID`, and several Python, Ruby and Go frameworks) read the client's value as
  the trusted identity: on Bypass and Static Requests always, and on Routed Requests the
  upstream may see both. The impact there is identity spoofing; Node/Express upstreams keep the
  names apart and are not affected.
- **Reference:** likely same (REQ-03 matches names case-insensitively, and Node accepts
  underscores in header names).

### SEC-06: spoofed forwarding headers reach the upstream

- **Test:** `security_headers::sec_06_spoofed_forwarding_headers_are_not_passed_upstream`.
- **Reproduction:** send `X-Real-IP`, `X-Forwarded-Host`, `X-Forwarded-Proto`,
  `X-Forwarded-Port`, `Forwarded`, `True-Client-IP` or `X-Client-IP`.
- **Observed:** each is forwarded unchanged; the gateway adds none of them (FWD-08).
  `X-Forwarded-For` is safe for an upstream that trusts one proxy: the gateway appends the real
  peer (`security_headers::the_last_forwarded_for_entry_is_the_real_peer`), but an upstream
  that reads its left-most entry (Express `trust proxy: true`) reads the client's value.
- **Safe:** headers that describe the client connection are set by the gateway or removed.
- **Severity: low.** Exploitable only where an upstream trusts these headers (logging, IP
  allow-lists, its own rate limiting, building absolute URLs or redirects from
  `X-Forwarded-Host`).
- **Reference:** same (FWD-07, FWD-08).

### SEC-07: headers nominated by `Connection` are forwarded

- **Test:** `security_headers::sec_07_headers_nominated_by_connection_are_not_forwarded`.
- **Reproduction:** `Connection: x-secret` with `X-Secret: s`.
- **Observed:** `x-secret` reaches the upstream (the fixed hop-by-hop list is removed; FWD-06).
- **Safe:** headers named in `Connection` are removed (RFC 9110 §7.6.1).
- **Severity: low.** The client already controls what it sends; the risk is an intermediary
  between gateway and upstream that honours the nomination differently.
- **Reference:** same (FWD-06: "Headers merely named inside `Connection` are still forwarded").

### SEC-08: authenticated and anonymous Identities share Buckets

- **Test:** `security_rate_limit::sec_08_authenticated_and_anonymous_identities_do_not_share_buckets`.
- **Reproduction:** an Identity resolver returns `Identity::authenticated(user)` for verified
  users and the client address, anonymously, for everyone else. A user named `127.0.0.1` calls
  a Route limited to 1 request a minute; then an anonymous client from `127.0.0.1` is answered
  429.
- **Observed:** Bucket keys are the Identity key as given, whether authenticated or not.
- **Safe:** authenticated and anonymous Identities count against separate Buckets.
- **Severity: low.** It needs a resolver that mixes both kinds of key and identity keys a
  client can choose (user names chosen at sign-up); the effect is spending another client's
  limits.
- **Reference:** likely same (RL-11, RL-12: the key is used as the resolver returns it).

## Covered: the gateway is safe

**Path handling** (`security_paths.rs`)

- `dot_segments_are_resolved_before_service_selection`: `..`, `%2e%2e`, `.%2E`, backslashes
  and climbing out of a Service Prefix, Bypass prefix or Direct Prefix give exactly the
  treatment of the resolved URL (service, Route, middleware and forwarded path agree).
- `encoded_traversal_in_routed_requests_is_never_forwarded`: `..%2f`, `%2e%2e%2f`, double
  encoding, `..%5c`, `..;`, `%00`, an encoded slash and overlong UTF-8 in Routed Requests are
  answered 400/405 and never forwarded.
- `control_bytes_in_the_request_target_are_rejected`: raw NUL and CR in the target are 400.
- `dot_segments_cannot_escape_a_bypass_prefix_or_static_space`: they are resolved before the
  request is classified.
- `a_doubled_slash_cannot_make_route_and_forwarded_path_disagree`: `/svc//x/…` matches no Route.
- `client_input_cannot_choose_the_upstream_host` (SSRF): absolute-form, scheme-relative and
  user-info targets, `Host`, `@` in the path, URLs in the query and `CONNECT` never make the
  gateway contact another address.
- `letter_case_variants_are_the_same_route`: case variants run through the Route's middleware.

**Header handling and smuggling** (`security_headers.rs`)

- `control_characters_in_header_lines_are_rejected`: bare CR, NUL, obs-fold, a line without a
  colon, a leading space and a space before the colon are 400.
- `bare_lf_line_endings_are_not_forwarded` (but see SEC-02).
- `encoded_crlf_never_reaches_the_upstream_decoded`; `middleware_header_values_with_line_breaks_are_refused` (503).
- `hop_by_hop_headers_are_not_forwarded`: the FWD-06 list, in any letter case.
- `ambiguous_framing_cannot_smuggle_a_request`: `Content-Length` with `Transfer-Encoding`,
  conflicting, listed, signed, hex and spaced lengths, nine obfuscated `Transfer-Encoding`
  forms, chunked HTTP/1.0 and a `GET` body: at most one response, the trailing request never
  forwarded, the upstream request framed once by the gateway.
- `an_upgrade_head_inside_a_body_is_not_an_upgrade`: the gateway's framing tracker stays in
  step with hyper.
- `the_last_forwarded_for_entry_is_the_real_peer`.
- `excluded_identity_headers_cannot_be_supplied_by_the_client`: any letter case, repeated, on
  Routed, Bypass and Static Requests.
- `oversize_heads_are_refused`: a 1 MiB header, a 1 MiB target and 200 header lines are 431.

**Rate limiting** (`security_rate_limit.rs`, Redis)

- `spoofed_client_address_headers_do_not_change_the_identity`: with the default `trust proxy`,
  `X-Forwarded-For`, `X-Real-IP`, `Forwarded` and four other client-address headers are ignored.
- `trusting_one_hop_ignores_addresses_the_client_prepends`.
- `path_variants_cannot_get_a_fresh_route_bucket`: case, trailing slash, dot segments,
  backslashes, query and fragment share the Bucket; encoded letters, doubled slashes, `;`,
  encoded slash, `%00` and a lower-case method match no Route.

**Resource limits** (`security_limits.rs`)

- `large_request_bodies_are_streamed_not_buffered`: a declared 100 MiB body is forwarded as it
  arrives.
- `overflowing_chunk_sizes_fail_the_request_only`.

**WebSocket** (`security_websocket.rs`)

- `plain_requests_are_rejected_by_the_middleware` (the precondition of SEC-01).
- `refused_upgrades_open_no_tunnel`: a non-101 answer closes the connection; a request
  pipelined behind the upgrade (WebSocket smuggling) never reaches the upstream.
- `non_websocket_upgrades_are_dropped`: `h2c` (h2c smuggling) and `POST` upgrades.
- `upgrade_targets_cannot_choose_the_upstream_host` (SSRF).
- `handshake_headers_reach_the_upstream_unchanged`: no key or version is invented, `Origin` is
  kept for the upstream to check, the excluded identity header is removed.
- `a_websocket_middleware_can_enforce_the_origin`: cross-site WebSocket hijacking can be
  stopped by the application.
- `large_frames_stream_through_the_tunnel`: frames are not buffered.

**Error responses** (`security_errors.rs`)

- `upstream_failures_do_not_reveal_upstream_addresses`: refused upstream, unloaded document,
  dead WebSocket upstream.
- `handler_failures_do_not_reveal_their_messages`: middleware and static-predicate errors and
  an invalid target answer the generic 500.
- `redis_failures_do_not_reveal_the_redis_address`.
- `reflected_input_stays_inside_json`: reflected segments are JSON-escaped, `<` and `>`
  arrive percent-encoded, the type is `application/json`.

Every error response checked carries no `Server` or `X-Powered-By` header.

## Observations (not findings)

- `Host` is forwarded unchanged (FWD-09). It never selects the upstream, but an upstream that
  routes by virtual host sees the client's value.
- A request with both `Content-Length` and `Transfer-Encoding: chunked` is accepted (hyper
  reads it as chunked and closes the connection afterwards); Node answers 400. Nothing can be
  smuggled either way.
- Request heads up to about 400 KiB and 100 header lines are accepted (hyper's defaults); Node
  accepts 16 KiB and 2000 lines.
- A malformed chunk size answers 503 `{"message":"Service unavailable."}` rather than 400.
- With `TrustProxy::All` every client chooses its own address through `X-Forwarded-For`, so
  IP-based Buckets can be bypassed; this is Express's `trust proxy: true` semantics and a
  deployment choice.
- The gateway does not validate the WebSocket handshake or `Origin`; the upstream or a
  WebSocket middleware does (see the covered tests).

## Not tested

- IPv6 clients (masking of IPv6 addresses to a /64 for IP Buckets): the tests serve on the IPv4
  loopback.
- TLS: client TLS and TLS upstreams.
- The `/mcp`, `/documents` and `/document-json` endpoints.
- Connection-count exhaustion and memory use under load: only per-connection timeouts are
  checked, within a few seconds each.
- How real upstream frameworks treat SEC-04 and SEC-05 inputs; the tests check what the
  gateway forwards.
- The reference's behaviour itself: the "Reference" column is judged from the Parity Checklist
  and Node's defaults, not from running the Conformance harness.
