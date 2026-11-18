# 09 OpenTelemetry

The reference ships a separate package, `@hodfords/nestjs-api-gateway-opentelemetry`, with one function that starts the Node SDK with auto-instrumentation. Rust has no auto-instrumentation, so the port provides explicit spans behind the cargo feature `otel` (decided in Q22). There is nothing to compare against the Reference Gateway beyond span names, so every row is tested Rust-only with an in-memory exporter.

| ID | Behaviour | Reference | Test | Status |
|---|---|---|---|---|
| OTEL-01 | One function starts tracing from an OTLP base URL and a service name. Spans are exported over OTLP/HTTP protobuf to `<url>/v1/traces`, and the resource carries `service.name`. | `libs/opentelemetry/tracing.ts:10-39`; `libs/opentelemetry/type.ts:1-5` | R | todo |
| OTEL-02 | Each proxied request produces a server span named `<METHOD> <pathname>`, for example `GET /user-service/v1/users`. | `libs/opentelemetry/tracing.ts:24-31` | R | todo |
| OTEL-03 | Each call to an Upstream Service produces a client span that is a child of the server span. | `libs/opentelemetry/tracing.ts:21` | R | todo |
| OTEL-04 | An incoming `traceparent` is continued, and `traceparent` is sent to the Upstream Service. | `libs/opentelemetry/tracing.ts:21` | R | todo |
| OTEL-05 | With the feature disabled or tracing not started, no span is created and no header is added. | none | R | todo |

Redis calls are not traced (Q22).
