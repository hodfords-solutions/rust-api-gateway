//! Rust-only tests of OpenTelemetry tracing (area 09, cargo feature `otel`): a real gateway
//! served in-process against mock upstreams, with spans collected by an in-memory exporter,
//! and the OTLP exporter posting to a local listener.
//!
//! Tracing state is process-wide, so the tests run one at a time ([`SERIAL`]).

#![cfg(feature = "otel")]

mod gateway_common;
mod rate_limit_common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::openapi::{DocumentFetcher, FetchFuture, FetchedDocument};
use api_gateway::otel::opentelemetry::trace::{SpanKind, Status};
use api_gateway::otel::opentelemetry::{SpanId, TraceId, Value};
use api_gateway::otel::{self, OpenTelemetryOptions, TracingGuard};
use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, TrustProxy};
use api_gateway::upstream::UpstreamService;
use axum::Router;
use axum::routing::get;
use gateway_common::{Upstream, client, options, serve};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT_ID: &str = "00f067aa0ba902b7";

/// Tracing started with an in-memory exporter that receives each span as it ends.
struct Collected {
    exporter: InMemorySpanExporter,
    _guard: TracingGuard,
}

fn start_tracing() -> Collected {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    Collected {
        exporter,
        _guard: otel::start_with_provider(provider),
    }
}

impl Collected {
    /// The ended spans, once there are at least `count` of them (and a moment for more).
    async fn spans(&self, count: usize) -> Vec<SpanData> {
        for _ in 0..200 {
            let spans = self.exporter.get_finished_spans().expect("spans");
            if spans.len() >= count {
                tokio::time::sleep(Duration::from_millis(50)).await;
                return self.exporter.get_finished_spans().expect("spans");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {count} spans, got {:?}",
            names(&self.exporter.get_finished_spans().expect("spans"))
        );
    }
}

fn names(spans: &[SpanData]) -> Vec<String> {
    spans.iter().map(|s| s.name.to_string()).collect()
}

fn one(spans: &[SpanData], kind: SpanKind) -> &SpanData {
    let matching: Vec<&SpanData> = spans.iter().filter(|s| s.span_kind == kind).collect();
    assert_eq!(matching.len(), 1, "one {kind:?} span in {:?}", names(spans));
    matching[0]
}

fn attribute(span: &SpanData, key: &str) -> Option<Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.clone())
}

fn string(span: &SpanData, key: &str) -> String {
    match attribute(span, key) {
        Some(value) => value.as_str().into_owned(),
        None => panic!("{} has no {key}: {:?}", span.name, span.attributes),
    }
}

fn int(span: &SpanData, key: &str) -> i64 {
    match attribute(span, key) {
        Some(Value::I64(value)) => value,
        other => panic!("{} has no integer {key}: {other:?}", span.name),
    }
}

fn traceparent(span: &SpanData) -> String {
    format!(
        "00-{}-{}-01",
        span.span_context.trace_id(),
        span.span_context.span_id()
    )
}

/// The value of the only header `name` the upstream received.
fn only(headers: &[(String, String)], name: &str) -> String {
    let values: Vec<&String> = headers
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v)
        .collect();
    assert_eq!(values.len(), 1, "one {name} in {headers:?}");
    values[0].clone()
}

/// Serves documents from memory, by URL.
struct Documents(HashMap<String, serde_json::Value>);

impl DocumentFetcher for Documents {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        let body = self
            .0
            .get(url)
            .map(|doc| doc.to_string().into_bytes())
            .unwrap_or_default();
        Box::pin(async move { Ok(FetchedDocument { status: 200, body }) })
    }
}

fn fetcher(document: serde_json::Value) -> Documents {
    Documents(HashMap::from([("mem://svc".to_string(), document)]))
}

/// OTEL-02, OTEL-03, OTEL-04: a proxied request has a server span named `<METHOD>
/// <pathname>` (the normalised path, no query) and a client span that is its child; the
/// upstream receives the client span's `traceparent`, last in the list, before the body's
/// framing.
#[tokio::test]
async fn otel_02_otel_03_proxied_request_spans() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;

    let response = client()
        .post(served.url("/svc/v1/x/../users?sig=secret&page=2"))
        .header("user-agent", "otel-test")
        .body("{}")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let spans = tracing.spans(2).await;
    assert_eq!(spans.len(), 2, "{:?}", names(&spans));
    let server = one(&spans, SpanKind::Server);
    let client = one(&spans, SpanKind::Client);

    assert_eq!(server.name, "POST /svc/v1/users");
    assert_eq!(server.parent_span_id, SpanId::INVALID);
    assert_eq!(string(server, "http.request.method"), "POST");
    assert_eq!(string(server, "http.route"), "/{*splat}");
    assert_eq!(string(server, "url.path"), "/svc/v1/users");
    assert_eq!(string(server, "url.query"), "sig=REDACTED&page=2");
    assert_eq!(string(server, "url.scheme"), "http");
    assert_eq!(string(server, "server.address"), "127.0.0.1");
    assert_eq!(int(server, "server.port"), i64::from(served.addr.port()));
    assert_eq!(string(server, "user_agent.original"), "otel-test");
    // Node's `socket.remoteAddress` on a dual-stack listener (FWD-07).
    assert_eq!(string(server, "network.peer.address"), "::ffff:127.0.0.1");
    assert_eq!(string(server, "client.address"), "::ffff:127.0.0.1");
    assert_eq!(int(server, "http.response.status_code"), 200);
    assert_eq!(server.status, Status::Unset);

    assert_eq!(client.name, "POST");
    assert_eq!(
        client.span_context.trace_id(),
        server.span_context.trace_id()
    );
    assert_eq!(client.parent_span_id, server.span_context.span_id());
    assert_eq!(
        string(client, "url.full"),
        format!("{}/v1/users/?sig=secret&page=2", upstream.origin())
    );
    assert_eq!(string(client, "url.path"), "/v1/users/");
    assert_eq!(string(client, "url.query"), "?sig=secret&page=2");
    assert_eq!(string(client, "server.address"), "127.0.0.1");
    assert_eq!(int(client, "server.port"), i64::from(upstream.addr.port()));
    assert_eq!(string(client, "user_agent.original"), "otel-test");
    assert_eq!(int(client, "http.response.status_code"), 200);
    assert_eq!(client.status, Status::Unset);

    let seen = upstream.seen();
    let headers = &seen.last().expect("forwarded").headers;
    assert_eq!(only(headers, "traceparent"), traceparent(client));
    let order: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        order[order.len() - 2..],
        ["traceparent", "content-length"],
        "{order:?}"
    );
}

/// OTEL-04: an incoming `traceparent` and `tracestate` are continued; the upstream receives
/// the gateway's, once each, in place of the client's.
#[tokio::test]
async fn otel_04_incoming_context_is_continued_and_propagated() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;

    let response = client()
        .get(served.url("/svc/v1/users"))
        .header("traceparent", format!("00-{TRACE_ID}-{PARENT_ID}-01"))
        .header("tracestate", "vendor=abc")
        .header("x-after", "1")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let spans = tracing.spans(2).await;
    let server = one(&spans, SpanKind::Server);
    let client = one(&spans, SpanKind::Client);
    let trace_id = TraceId::from_hex(TRACE_ID).expect("trace id");
    assert_eq!(server.span_context.trace_id(), trace_id);
    assert_eq!(
        server.parent_span_id,
        SpanId::from_hex(PARENT_ID).expect("span id")
    );
    assert!(server.parent_span_is_remote);
    assert_eq!(client.span_context.trace_id(), trace_id);
    assert_eq!(client.parent_span_id, server.span_context.span_id());

    let seen = upstream.seen();
    let headers = &seen.last().expect("forwarded").headers;
    assert_eq!(only(headers, "traceparent"), traceparent(client));
    assert_eq!(only(headers, "tracestate"), "vendor=abc");
    let order: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        order[order.len() - 2..],
        ["traceparent", "tracestate"],
        "{order:?}"
    );
}

/// OTEL-03: an unreachable upstream is recorded on the client span (error status,
/// `error.type`, an `exception` event); the server span carries the 503.
#[tokio::test]
async fn otel_03_upstream_failure_is_recorded() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = closed.local_addr().expect("address").port();
    drop(closed);
    let document = json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": { "/v1/users": { "get": { "operationId": "listUsers" } } }
    });
    let service = UpstreamService::new("svc", "mem://svc", format!("http://127.0.0.1:{port}"));
    let served = serve(
        Gateway::builder(options(vec![service])).document_fetcher(fetcher(document)),
        true,
    )
    .await;

    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 503);
    let _ = response.text().await;

    let spans = tracing.spans(2).await;
    let server = one(&spans, SpanKind::Server);
    let client = one(&spans, SpanKind::Client);
    assert_eq!(client.parent_span_id, server.span_context.span_id());
    assert_eq!(string(client, "error.type"), "ECONNREFUSED");
    assert!(
        matches!(client.status, Status::Error { .. }),
        "{:?}",
        client.status
    );
    assert!(
        client.events.iter().any(|event| event.name == "exception"),
        "{:?}",
        client.events
    );
    assert!(attribute(client, "http.response.status_code").is_none());
    assert_eq!(server.name, "GET /svc/v1/users");
    assert_eq!(int(server, "http.response.status_code"), 503);
    assert_eq!(string(server, "error.type"), "503");
    assert!(matches!(server.status, Status::Error { .. }));
}

/// OTEL-02: the gateway's endpoints and the application's routes are named by their route;
/// a response Express gives before routing keeps the bare method. None of them calls an
/// upstream.
#[tokio::test]
async fn otel_02_endpoints_and_application_routes() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.documents.enabled = true;
    let routes = Router::new().route("/hello/{name}", get(|| async { "hi" }));
    let served = serve(Gateway::builder(gateway_options).routes(routes), true).await;

    for (path, status) in [
        ("/DOCUMENTS/", 200),
        ("/document-json", 404),
        ("/hello/ann", 200),
        ("/svc/%zz", 400),
    ] {
        let response = client()
            .get(served.url(path))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), status, "{path}");
        let _ = response.bytes().await;
    }

    let spans = tracing.spans(4).await;
    assert!(
        spans.iter().all(|s| s.span_kind == SpanKind::Server),
        "{:?}",
        names(&spans)
    );
    let mut got = names(&spans);
    got.sort();
    assert_eq!(
        got,
        [
            "GET",
            "GET /document-json",
            "GET /documents",
            "GET /hello/{name}"
        ]
    );
    let route = |name: &str| {
        let span = spans.iter().find(|s| s.name == name).expect("span");
        attribute(span, "http.route").map(|v| v.as_str().into_owned())
    };
    assert_eq!(route("GET /documents").as_deref(), Some("/documents"));
    assert_eq!(route("GET /hello/{name}").as_deref(), Some("/hello/{name}"));
    assert_eq!(route("GET"), None);
}

/// OTEL-02, OTEL-03, OTEL-04: a WebSocket upgrade has no server span; its upstream handshake
/// has a client span, a child of the client's trace context, whose `traceparent` replaces
/// the client's in place.
#[tokio::test]
async fn otel_03_websocket_upgrade_has_a_client_span_only() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = RawUpstream::start().await;
    let document = json!({
        "openapi": "3.0.0",
        "info": { "title": "ws", "version": "1" },
        "paths": { "/v1/users": { "get": { "operationId": "listUsers" } } }
    });
    let service = UpstreamService::new("svc", "mem://svc", format!("http://{}", upstream.addr));
    let served = serve(
        Gateway::builder(options(vec![service])).document_fetcher(fetcher(document)),
        true,
    )
    .await;

    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(
            format!(
                "GET /svc/socket HTTP/1.1\r\nHost: gw\r\ntraceparent: 00-{TRACE_ID}-{PARENT_ID}-01\r\n\
                 Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send");
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
    assert!(
        answer.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"),
        "{:?}",
        String::from_utf8_lossy(&answer)
    );

    let spans = tracing.spans(1).await;
    assert_eq!(spans.len(), 1, "{:?}", names(&spans));
    let client = one(&spans, SpanKind::Client);
    assert_eq!(client.name, "GET");
    assert_eq!(
        client.span_context.trace_id(),
        TraceId::from_hex(TRACE_ID).expect("trace id")
    );
    assert_eq!(
        client.parent_span_id,
        SpanId::from_hex(PARENT_ID).expect("span id")
    );
    assert_eq!(int(client, "http.response.status_code"), 101);
    assert_eq!(
        string(client, "url.full"),
        format!("http://{}/svc/socket", upstream.addr)
    );

    let head = upstream.heads().pop().expect("upgrade request");
    let lines: Vec<&str> = head.split("\r\n").collect();
    assert_eq!(lines[0], "GET /svc/socket HTTP/1.1");
    assert_eq!(lines[1], "host: gw");
    assert_eq!(lines[2], format!("traceparent: {}", traceparent(client)));
    assert_eq!(
        head.matches("traceparent").count(),
        1,
        "one traceparent in {head:?}"
    );
}

/// OTEL-02, OTEL-03: an MCP tool call's upstream request has a client span, a child of the
/// `POST /mcp` server span, and carries its `traceparent` after the call's headers.
#[cfg(feature = "mcp")]
#[tokio::test]
async fn otel_03_mcp_tool_call_spans() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let document = json!({
        "openapi": "3.0.0",
        "info": { "title": "mcp", "version": "1" },
        "paths": {
            "/users/{id}": {
                "get": {
                    "operationId": "getUser",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "x-tenant", "in": "header", "schema": { "type": "string" } }
                    ]
                }
            }
        }
    });
    let mut gateway_options = options(vec![UpstreamService::new(
        "svc",
        "mem://svc",
        upstream.origin(),
    )]);
    gateway_options.mcp = Some(api_gateway::mcp::McpOptions::enabled());
    let served = serve(
        Gateway::builder(gateway_options).document_fetcher(fetcher(document)),
        true,
    )
    .await;
    let mut tools = Vec::new();
    for _ in 0..100 {
        tools = served
            .gateway
            .mcp_tools()
            .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default();
        if !tools.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let tool = tools.first().expect("a tool").clone();

    let response = client()
        .post(served.url("/mcp"))
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": tool, "arguments": { "path_id": "42", "header_x-tenant": "t1" } }
            })
            .to_string(),
        )
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let text = response.text().await.expect("body");
    assert!(text.contains("upstream /users/42"), "{text}");

    let spans = tracing.spans(2).await;
    let server = one(&spans, SpanKind::Server);
    let client = one(&spans, SpanKind::Client);
    assert_eq!(server.name, "POST /mcp");
    assert_eq!(string(server, "http.route"), "/mcp");
    assert_eq!(client.name, "GET");
    assert_eq!(client.parent_span_id, server.span_context.span_id());
    assert_eq!(
        client.span_context.trace_id(),
        server.span_context.trace_id()
    );
    assert_eq!(int(client, "http.response.status_code"), 200);
    assert_eq!(
        string(client, "url.full"),
        format!("{}/users/42/", upstream.origin())
    );

    let seen = upstream.seen();
    let headers = &seen.last().expect("tool call").headers;
    assert_eq!(only(headers, "traceparent"), traceparent(client));
    let order: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(order.last(), Some(&"traceparent"), "{order:?}");
}

/// OTEL-05: before tracing starts and after its guard is dropped, no span is created and the
/// client's trace headers are forwarded untouched, like any other header.
#[tokio::test]
async fn otel_05_nothing_when_tracing_is_not_started() {
    let _serial = SERIAL.lock().await;
    assert!(!otel::is_started());
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    let incoming = format!("00-{TRACE_ID}-{PARENT_ID}-01");
    let send = || async {
        let response = client()
            .get(served.url("/svc/v1/users"))
            .header("traceparent", incoming.as_str())
            .header("x-after", "1")
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), 200);
        let _ = response.text().await;
        let seen = upstream.seen();
        seen.last().expect("forwarded").headers.clone()
    };

    // A provider that was built but never started sees nothing.
    let unused = InMemorySpanExporter::default();
    let _provider = SdkTracerProvider::builder()
        .with_simple_exporter(unused.clone())
        .build();
    let before = send().await;
    assert_eq!(only(&before, "traceparent"), incoming);
    let order: Vec<&str> = before.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        order,
        [
            "host",
            "connection",
            "traceparent",
            "x-after",
            "accept",
            "x-forwarded-for"
        ]
    );
    assert!(!before.iter().any(|(n, _)| n == "tracestate"));

    // Started, then stopped: nothing after the guard is dropped.
    let tracing = start_tracing();
    assert!(otel::is_started());
    let exporter = tracing.exporter.clone();
    drop(tracing);
    assert!(!otel::is_started());
    let after = send().await;
    assert_eq!(after, before);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(exporter.get_finished_spans().expect("spans").is_empty());
    assert!(unused.get_finished_spans().expect("spans").is_empty());
}

/// OTEL-01: `start` exports over OTLP/HTTP protobuf to `<url>/v1/traces`, with
/// `service.name` on the resource; shutting the guard down flushes the batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otel_01_otlp_exporter_posts_protobuf() {
    let _serial = SERIAL.lock().await;
    let collector = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = collector.local_addr().expect("address").port();
    let received = tokio::spawn(async move {
        let (mut stream, _) = collector.accept().await.expect("export connection");
        let request = read_request(&mut stream).await;
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n")
            .await;
        request
    });

    let options =
        OpenTelemetryOptions::new(format!("http://127.0.0.1:{port}"), "otel-test-service");
    assert_eq!(
        options.traces_endpoint(),
        format!("http://127.0.0.1:{port}/v1/traces")
    );
    let guard = otel::start(&options).expect("tracing starts");
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(gateway_common::options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::task::spawn_blocking(move || guard.shutdown())
        .await
        .expect("shutdown thread")
        .expect("shutdown");

    let (head, body) = tokio::time::timeout(Duration::from_secs(10), received)
        .await
        .expect("an export arrives")
        .expect("collector task");
    let head = head.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/traces http/1.1\r\n"), "{head}");
    assert!(
        head.contains("\r\ncontent-type: application/x-protobuf\r\n"),
        "{head}"
    );
    let contains = |needle: &str| body.windows(needle.len()).any(|w| w == needle.as_bytes());
    assert!(contains("service.name"), "the resource");
    assert!(contains("otel-test-service"), "the service name");
    assert!(contains("GET /svc/v1/users"), "the server span");
}

/// The document of a service `svc` with one operation, `GET /v1/users`.
fn users_document() -> serde_json::Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "t", "version": "1" },
        "paths": { "/v1/users": { "get": { "operationId": "listUsers" } } }
    })
}

/// D14 (OTEL-04): a WebSocket handshake's client span continues the client's trace (the
/// reference's starts a new trace) and ends when the upstream's `101` arrives, while the
/// tunnel is still open (the reference's lasts until the tunnel closes).
#[tokio::test]
async fn d14_websocket_handshake_span_continues_the_trace_and_ends_at_the_handshake() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = RawUpstream::start_with(true).await;
    let service = UpstreamService::new("svc", "mem://svc", format!("http://{}", upstream.addr));
    let served = serve(
        Gateway::builder(options(vec![service])).document_fetcher(fetcher(users_document())),
        true,
    )
    .await;

    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(
            format!(
                "GET /svc/socket HTTP/1.1\r\nHost: gw\r\ntraceparent: 00-{TRACE_ID}-{PARENT_ID}-01\r\n\
                 Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("send");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .expect("the 101 arrives")
            .expect("read");
        assert_eq!(
            n,
            1,
            "closed before the 101: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"));

    // The span has ended although the tunnel is open.
    let spans = tracing.spans(1).await;
    assert_eq!(spans.len(), 1, "{:?}", names(&spans));
    let client = one(&spans, SpanKind::Client);
    assert_eq!(
        client.span_context.trace_id(),
        TraceId::from_hex(TRACE_ID).expect("trace id")
    );
    assert_eq!(
        client.parent_span_id,
        SpanId::from_hex(PARENT_ID).expect("span id")
    );
    assert!(client.parent_span_is_remote);
    assert_eq!(int(client, "http.response.status_code"), 101);
    let mut buffer = [0u8; 16];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buffer))
            .await
            .is_err(),
        "the tunnel is still open"
    );
    stream.write_all(b"still open").await.expect("tunnel write");
    drop(stream);
}

/// D14 (OTEL-04): only W3C Trace Context is propagated. A client's `baggage` (and any other
/// tracing header) reaches the upstream unchanged, in its place, like any other header; the
/// upstream receives one `traceparent`, the gateway's (see also
/// `otel_04_incoming_context_is_continued_and_propagated`).
#[tokio::test]
async fn d14_only_w3c_trace_context_is_propagated() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    let baggage = "k1=v1,k2=v2;p=1";
    let b3 = format!("{TRACE_ID}-{PARENT_ID}-1");
    let response = client()
        .get(served.url("/svc/v1/users"))
        .header("traceparent", format!("00-{TRACE_ID}-{PARENT_ID}-01"))
        .header("baggage", baggage)
        .header("b3", b3.as_str())
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let spans = tracing.spans(2).await;
    let client = one(&spans, SpanKind::Client);
    let seen = upstream.seen();
    let headers = &seen.last().expect("forwarded").headers;
    assert_eq!(only(headers, "traceparent"), traceparent(client));
    assert_eq!(only(headers, "baggage"), baggage);
    assert_eq!(only(headers, "b3"), b3);
    let order: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        order,
        [
            "host",
            "connection",
            "baggage",
            "b3",
            "accept",
            "x-forwarded-for",
            "traceparent"
        ]
    );
}

/// D14 (OTEL-02, OTEL-03): attributes the reference's instrumentation sets and the port
/// does not: `client.address` from `Forwarded`, `server.address` and `url.scheme` from
/// `Forwarded` or `X-Forwarded-Host`, `network.peer.*` on client spans; and `url.query` is
/// the query as received apart from the redacted values (the reference re-serialises it).
#[tokio::test]
async fn d14_attributes_left_out() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    let response = client()
        .get(served.url("/svc/v1/users?q=a%20b&flag&sig=s"))
        .header(
            "forwarded",
            "for=192.0.2.1;host=forwarded.example;proto=https",
        )
        .header("x-forwarded-host", "xfh.example")
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let spans = tracing.spans(2).await;
    let server = one(&spans, SpanKind::Server);
    let client = one(&spans, SpanKind::Client);
    assert_eq!(string(server, "client.address"), "::ffff:127.0.0.1");
    assert_eq!(string(server, "server.address"), "127.0.0.1");
    assert_eq!(string(server, "url.scheme"), "http");
    assert_eq!(string(server, "url.query"), "q=a%20b&flag&sig=REDACTED");
    for key in ["network.peer.address", "network.peer.port"] {
        assert!(attribute(server, key).is_some(), "server span {key}");
        assert!(attribute(client, key).is_none(), "client span {key}");
    }
}

/// D14 (OTEL-03): an upstream that fails in the middle of its body ends the client span
/// without an error status (the reference's undici instrumentation records the error).
#[tokio::test]
async fn d14_upstream_failing_mid_body_leaves_the_client_span_unset() {
    let _serial = SERIAL.lock().await;
    let tracing = start_tracing();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte).await {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
                    .await;
            });
        }
    });
    let service = UpstreamService::new("svc", "mem://svc", format!("http://127.0.0.1:{port}"));
    let served = serve(
        Gateway::builder(options(vec![service])).document_fetcher(fetcher(users_document())),
        true,
    )
    .await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .expect("response");
    assert_eq!(response.status(), 200);
    assert!(response.bytes().await.is_err(), "the body is cut short");

    let spans = tracing.spans(2).await;
    let client = one(&spans, SpanKind::Client);
    assert_eq!(client.status, Status::Unset);
    assert!(attribute(client, "error.type").is_none());
    assert_eq!(int(client, "http.response.status_code"), 200);
}

/// D14 (Q22): only what the gateway instruments has spans. Rate-limited requests make
/// Redis round trips (the second is refused by its Bucket), yet the traces hold the
/// gateway's server and client spans alone (the reference also has Express, NestJS, DNS,
/// TCP and Redis spans).
#[tokio::test]
async fn d14_only_the_gateway_s_own_spans() {
    let _serial = SERIAL.lock().await;
    if !rate_limit_common::redis_up().await {
        return;
    }
    let tracing = start_tracing();
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(1, 60.0),
        BucketLimit::new(1_000_000, 60.0),
    );
    gateway_options.rate_limit.trust_proxy = TrustProxy::All;
    gateway_options.redis = rate_limit_common::redis_options();
    let served = serve(Gateway::builder(gateway_options), true).await;
    // A client address of its own, so the Bucket is this test's alone.
    let address = format!("otel-{}", rate_limit_common::nonce());
    for status in [200, 429] {
        let response = client()
            .get(served.url("/svc/v1/users"))
            .header("x-forwarded-for", address.as_str())
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), status);
        let _ = response.text().await;
    }
    let spans = tracing.spans(3).await;
    let mut kinds: Vec<String> = spans
        .iter()
        .map(|s| format!("{:?} {}", s.span_kind, s.name))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            "Client GET",
            "Server GET /svc/v1/users",
            "Server GET /svc/v1/users"
        ]
    );
}

/// Reads one request: its head (as text) and its body (by `content-length`).
async fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = stream.read(&mut chunk).await.expect("read");
        assert!(n > 0, "connection closed inside the head");
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .expect("a content-length");
    let mut body = buffer[end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.expect("read");
        assert!(n > 0, "connection closed inside the body");
        body.extend_from_slice(&chunk[..n]);
    }
    (head, body)
}

/// An upstream that records each request head, answers `101`, then closes (or, with
/// [`RawUpstream::start_with`], keeps the tunnel open).
struct RawUpstream {
    addr: SocketAddr,
    heads: Arc<Mutex<Vec<String>>>,
}

impl RawUpstream {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// With `keep_open`, the upstream keeps the tunnel open after its `101` until the gateway
    /// closes it.
    async fn start_with(keep_open: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let heads = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&heads);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte).await {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    recorded
                        .lock()
                        .expect("heads")
                        .push(String::from_utf8_lossy(&head).into_owned());
                    let _ = stream
                        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                        .await;
                    if keep_open {
                        let mut buffer = [0u8; 1024];
                        while matches!(stream.read(&mut buffer).await, Ok(n) if n > 0) {}
                    }
                });
            }
        });
        Self { addr, heads }
    }

    fn heads(&self) -> Vec<String> {
        self.heads.lock().expect("heads").clone()
    }
}
