# 09 OpenTelemetry

The reference ships a separate package, `@hodfords/nestjs-api-gateway-opentelemetry`, with one function that starts the Node SDK with auto-instrumentation. Rust has no auto-instrumentation, so the port provides explicit spans behind the cargo feature `otel` (decided in Q22), in the module `api_gateway::otel`. There is nothing to compare against the Reference Gateway beyond span names, so every row is tested Rust-only with an in-memory exporter (`crates/rust-gateway/tests/otel.rs`).

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| OTEL-01 | One function starts tracing from an OTLP base URL and a service name. Spans are exported over OTLP/HTTP protobuf to `<url>/v1/traces`, and the resource carries `service.name`. In the port: `otel::start(&OpenTelemetryOptions { otlp_url, service_name })`, returning a guard that flushes and shuts the exporter down when dropped. | `libs/opentelemetry/tracing.ts:10-39`; `libs/opentelemetry/type.ts:1-5` | R (`otel::otel_01_otlp_exporter_posts_protobuf`) | done |
| OTEL-02 | Each proxied request produces a server span named `<METHOD> <pathname>`, for example `GET /user-service/v1/users`. The pathname is the request's path after URL normalisation (REQ-02: the reference's hook reads `req.url`, which the sanitiser has rewritten by then), without the query. Other requests: see [Which requests get which spans](#which-requests-get-which-spans). **Deviation D14:** only what the gateway instruments has spans, and some attributes are left out; see [Differences](#differences-from-the-references-auto-instrumentation-deviation-d14). | `libs/opentelemetry/tracing.ts:24-31`; `normalizeRequestUrl` in `restful/helpers/request-sanitizer.helper.ts:15-20` | R (`otel::otel_02_otel_03_proxied_request_spans`, `otel::otel_02_endpoints_and_application_routes`, `otel::otel_03_websocket_upgrade_has_a_client_span_only`, `otel::d14_only_the_gateway_s_own_spans`, `otel::d14_attributes_left_out`) | deviation |
| OTEL-03 | Each call to an Upstream Service produces a client span that is a child of the server span: forwarded requests and MCP tool calls. A failure to reach the upstream is recorded on it (error status, `error.type`, an `exception` event). **Deviation D14:** a WebSocket handshake's client span ends at the handshake, and a failure in the middle of the upstream's body is not recorded; see [Differences](#differences-from-the-references-auto-instrumentation-deviation-d14). | `libs/opentelemetry/tracing.ts:21` | R (`otel::otel_02_otel_03_proxied_request_spans`, `otel::otel_03_upstream_failure_is_recorded`, `otel::otel_03_mcp_tool_call_spans`, `otel::otel_03_websocket_upgrade_has_a_client_span_only`, `otel::d14_websocket_handshake_span_continues_the_trace_and_ends_at_the_handshake`, `otel::d14_upstream_failing_mid_body_leaves_the_client_span_unset`, `otel::d14_attributes_left_out`) | deviation |
| OTEL-04 | An incoming `traceparent` is continued, and `traceparent` is sent to the Upstream Service. See [Where the trace headers land](#where-the-trace-headers-land). **Deviation D14:** the upstream receives one `traceparent` when the client sent one (the reference's undici sends two), a WebSocket handshake continues the client's trace (the reference's starts a new one), and only W3C Trace Context is propagated; see [Differences](#differences-from-the-references-auto-instrumentation-deviation-d14). | `libs/opentelemetry/tracing.ts:21` | R (`otel::otel_04_incoming_context_is_continued_and_propagated`, `otel::otel_02_otel_03_proxied_request_spans`, `otel::otel_03_mcp_tool_call_spans`, `otel::otel_03_websocket_upgrade_has_a_client_span_only`, `otel::d14_websocket_handshake_span_continues_the_trace_and_ends_at_the_handshake`, `otel::d14_only_w3c_trace_context_is_propagated`) | deviation |
| OTEL-05 | With the feature disabled or tracing not started, no span is created and no header is added. Tracing stops again when the guard is dropped. | none | R (`otel::otel_05_nothing_when_tracing_is_not_started`); the Conformance Tests run against `rust-gateway-server` built with the feature, tracing not started, and without it | done |

Redis calls are not traced (Q22).

## Which requests get which spans

What the reference's auto-instrumentation produces decides it: `@opentelemetry/instrumentation-http` starts a server span for every `request` event of Node's server, names it `<METHOD> <http.route>` when Express reports a route, and the package's hook then renames spans whose name ends in the catch-all route `/{*splat}` to `<METHOD> <pathname>`.

| Request | Server span name | `http.route` | Client span |
|---|---|---|---|
| Proxied (Routed, Static and Bypass Requests, and the proxy's own errors) | `<METHOD> <pathname>` | `/{*splat}` | `<METHOD>`, when the request is forwarded |
| `/documents`, `/document-json` (any case, one trailing slash) | `GET /documents`, `GET /document-json` (`HEAD` for a `HEAD`) | the same path | none |
| `POST /mcp` | `POST /mcp` | `/mcp` | `<METHOD>` per tool call that reaches the upstream |
| An application route | `<METHOD> <route>`, axum's matched path such as `/users/{id}` (Express would write `/users/:id`) | the matched path | none from the gateway |
| Answered before routing (Express's 400 for a malformed `%`, the asterisk-form target) | `<METHOD>` | none | none |
| WebSocket upgrade | none: Node emits `upgrade`, not `request`, so no server span starts | | `GET` for the upstream handshake |
| Rejected by the HTTP parser | none | | none |

Client spans are named as undici's and Node's HTTP client instrumentations name them: the method alone (`HTTP` for a method outside the standard ones).

Attributes follow the stable HTTP semantic conventions as `@opentelemetry/instrumentation-http` 0.300 and `@opentelemetry/instrumentation-undici` 0.34 set them. Server span: `http.request.method` (`_OTHER` for unknown methods, with `http.request.method_original`), `url.scheme`, `url.path`, `url.query` (with the reference's default redacted parameters), `server.address` and `server.port` (from `Host`), `network.peer.address` and `network.peer.port` (Node's form, `::ffff:` for IPv4 on a dual-stack listener), `network.protocol.version`, `client.address` (first `X-Forwarded-For` entry, else the peer), `user_agent.original`, `http.response.status_code`, `http.route`; error status and `error.type` for 5xx. Client span: `http.request.method`, `http.request.method_original`, `url.full`, `url.path`, `url.query`, `url.scheme`, `server.address`, `server.port`, `user_agent.original`, `http.response.status_code`; error status and `error.type` for 4xx and 5xx, or the failure's code (`ECONNREFUSED`, `UND_ERR_HEADERS_TIMEOUT`, ...) when no response arrived.

The server span ends when the response body has been sent (or dropped); a forwarded request's client span ends with the upstream body, a tool call's once its body is read, a WebSocket handshake's when the upstream's response head arrives.

## Where the trace headers land

Only while tracing is started. The client's `traceparent` and `tracestate` are dropped and the gateway's (`traceparent`, then `tracestate` when the trace state is not empty) take their place:

- **Forwarded requests**: appended after every other header of the list, before `content-length` or `transfer-encoding`, where undici's instrumentation adds them with `request.addHeader`.
- **MCP tool calls**: after the call's headers, before `content-length`, for the same reason.
- **WebSocket upgrades**: set on the headers object as Node's HTTP instrumentation sets them, replacing the client's `traceparent` in place, appended after the middleware headers otherwise, before the `Host` and `Connection` Node adds.

## Differences from the reference's auto-instrumentation (Deviation D14)

Approved on 2026-11-18 as Deviation D14 (rows OTEL-02 to OTEL-04). Each difference is pinned by a Rust-only test in `crates/rust-gateway/tests/otel.rs`, so a change to any of them fails a test.

- No spans for what Rust does not instrument: Express middleware and request-handler spans, NestJS controller spans, DNS and TCP spans, Redis commands (Q22). Tests: `otel::d14_only_the_gateway_s_own_spans` (rate-limited requests through Redis have the server and client spans alone), and the exact span lists of `otel::otel_02_otel_03_proxied_request_spans` and `otel::otel_02_endpoints_and_application_routes`.
- A client that sends `traceparent` makes the reference's undici forward it and append its own, so the upstream receives two `traceparent` headers (Node joins them into an invalid value and the upstream starts a new trace). The port sends one. Tests: `otel::otel_04_incoming_context_is_continued_and_propagated`, `otel::d14_only_w3c_trace_context_is_propagated`.
- A WebSocket upgrade's client span in the reference starts a new trace (the `upgrade` handler runs outside any request context) and ends when the tunnel closes; the port continues the client's trace and ends the span at the handshake. Tests: `otel::d14_websocket_handshake_span_continues_the_trace_and_ends_at_the_handshake` (the span has ended while the tunnel is open), `otel::otel_03_websocket_upgrade_has_a_client_span_only`.
- Only W3C Trace Context is propagated. A `baggage` header is forwarded like any other header, not re-serialised. Test: `otel::d14_only_w3c_trace_context_is_propagated`.
- Attributes not set: `network.peer.*` on client spans, `client.address` from a `Forwarded` header, `server.address` from `Forwarded` or `X-Forwarded-Host`; `url.query` keeps the query as received apart from the redacted values (the reference re-serialises it with `URLSearchParams`). Test: `otel::d14_attributes_left_out`.
- An upstream that fails in the middle of its body ends the client span without an error status. Test: `otel::d14_upstream_failing_mid_body_leaves_the_client_span_unset`.
- `OpenTelemetryOptions.minDuration` is not ported (declared, never read). Nothing to test at run time: `OpenTelemetryOptions` has no such field.
