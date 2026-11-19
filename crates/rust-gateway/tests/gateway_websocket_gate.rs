//! Rust-only tests of Deviation D15 (WS-06, WS-15, WS-17, WS-18): an upgrade never goes
//! through the Route lookup or the proxy middleware, so `Connection: Upgrade` +
//! `Upgrade: websocket` must not be a way around them. Upgrades are accepted only under
//! `websocket_route_prefixes`, count against the IP Buckets, and an upstream that does not
//! switch protocols gets the client a 502, never its own answer. The reference before 12.5.0
//! forwards every upgrade and relays the upstream's answer (`deviations::d15_*`).

mod gateway_common;
mod rate_limit_common;
mod websocket_common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use api_gateway::gateway::{Gateway, GatewayOptions};
use api_gateway::handler::{GatewayRequest, MiddlewareError, WsProxyMiddleware, async_trait};
use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, TrustProxy};
use api_gateway::request::ProxyRequest;
use api_gateway::upstream::UpstreamService;
use gateway_common::{options, serve};
use rate_limit_common::nonce;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use websocket_common::{EchoUpstream, read_head, read_to_close, upgrade_request};

const FORBIDDEN: &str = r#"{"message":"Forbidden","statusCode":403}"#;

/// Allows unless `x-ws: deny`; counts its calls.
#[derive(Clone, Default)]
struct Counting {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl WsProxyMiddleware for Counting {
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(request.header("x-ws") != Some("deny"))
    }
}

fn service(addr: SocketAddr) -> UpstreamService {
    UpstreamService::new(
        "svc",
        format!("http://{addr}/openapi.json"),
        format!("http://{addr}"),
    )
}

fn gate(addr: SocketAddr, prefixes: &[&str]) -> GatewayOptions {
    let mut options = options(vec![service(addr)]);
    options.websocket_route_prefixes = prefixes.iter().map(|p| p.to_string()).collect();
    options
}

async fn exchange(addr: SocketAddr, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request).await.expect("send");
    String::from_utf8_lossy(&read_to_close(&mut stream).await).into_owned()
}

fn status_and_body(response: &str) -> (&str, &str) {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .expect("a complete response");
    (head.split("\r\n").next().unwrap_or_default(), body)
}

/// WS-17: only paths under `websocket_route_prefixes` (whole segments, after normalisation)
/// are upgraded; every other upgrade is answered 403 and never reaches the upstream or the
/// middleware.
#[tokio::test]
async fn d15_upgrades_are_accepted_only_on_websocket_routes() {
    let upstream = EchoUpstream::start().await;
    let middleware = Counting::default();
    let served = serve(
        Gateway::builder(gate(upstream.addr, &["svc/socket.io"]))
            .websocket_middleware(middleware.clone()),
        true,
    )
    .await;
    for path in [
        "/svc/v1/users",
        "/svc/v1/admin/users",
        "/svc/socket.io-admin/x",
        "/svc/socket.io/../v1/admin/users",
        "/svc/socket.io/%2e%2e/v1/admin/users",
        "/svc",
    ] {
        let response = exchange(served.addr, &upgrade_request(path, "")).await;
        assert_eq!(
            status_and_body(&response),
            ("HTTP/1.1 403 Forbidden", FORBIDDEN),
            "{path}"
        );
    }
    assert_eq!(upstream.accepted(), 0, "nothing reached the upstream");
    assert_eq!(
        middleware.calls.load(Ordering::SeqCst),
        0,
        "no middleware ran"
    );

    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(&upgrade_request(
            "/svc/socket.io/?EIO=4&transport=websocket",
            "",
        ))
        .await
        .expect("send");
    let head = read_head(&mut stream).await;
    assert!(
        head.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    assert_eq!(upstream.accepted(), 1);
    assert_eq!(middleware.calls.load(Ordering::SeqCst), 1);
}

/// WS-17: with no `websocket_route_prefixes` (the default), no upgrade is accepted.
#[tokio::test]
async fn d15_without_websocket_routes_every_upgrade_is_refused() {
    let upstream = EchoUpstream::start().await;
    let served = serve(Gateway::builder(gate(upstream.addr, &[])), true).await;
    let response = exchange(served.addr, &upgrade_request("/svc/socket", "")).await;
    assert_eq!(
        status_and_body(&response),
        ("HTTP/1.1 403 Forbidden", FORBIDDEN)
    );
    assert_eq!(upstream.accepted(), 0);
}

/// WS-06: a request that is not a WebSocket handshake is dropped before anything else, so
/// no middleware runs for it and nothing is counted.
#[tokio::test]
async fn d15_a_non_websocket_upgrade_is_dropped_before_the_middleware() {
    let upstream = EchoUpstream::start().await;
    let middleware = Counting::default();
    let served = serve(
        Gateway::builder(gate(upstream.addr, &["svc"])).websocket_middleware(middleware.clone()),
        true,
    )
    .await;
    let mut post = upgrade_request("/svc/socket", "x-ws: deny\r\n");
    post.splice(0..3, b"POST".iter().copied());
    assert_eq!(exchange(served.addr, &post).await, "");
    assert_eq!(middleware.calls.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.accepted(), 0);
}

/// An upstream that serves its document and answers every other request as plain HTTP with
/// a secret body, as an HTTP API answers an upgrade it does not support.
async fn plain_http_upstream(response: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
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
                let answer = if head.starts_with(b"GET /openapi.json ") {
                    let body = websocket_common::document();
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    response.to_string()
                };
                let _ = stream.write_all(answer.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

/// WS-15: an upstream that answers without switching protocols gets the client a 502 with
/// the gateway's own body; the upstream's status, headers and body are never passed on.
#[tokio::test]
async fn d15_an_upstream_that_does_not_switch_protocols_gets_a_502() {
    for (label, answer) in [
        (
            "200 with content-length",
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-internal: yes\r\ncontent-length: 20\r\n\r\n{\"users\":[\"secret\"]}",
        ),
        (
            "200 until close",
            "HTTP/1.1 200 OK\r\nx-internal: yes\r\n\r\n{\"users\":[\"secret\"]}",
        ),
        (
            "401 chunked",
            "HTTP/1.1 401 Unauthorized\r\nx-internal: yes\r\ntransfer-encoding: chunked\r\n\r\n6\r\nsecret\r\n0\r\n\r\n",
        ),
    ] {
        let upstream = plain_http_upstream(answer).await;
        let served = serve(Gateway::builder(gate(upstream, &["svc"])), true).await;
        let response = exchange(served.addr, &upgrade_request("/svc/v1/users", "")).await;
        assert_eq!(
            status_and_body(&response),
            (
                "HTTP/1.1 502 Bad Gateway",
                r#"{"statusCode":502,"message":"Upstream did not switch protocols."}"#
            ),
            "{label}"
        );
        assert!(!response.contains("secret"), "{label}: {response}");
        assert!(!response.contains("x-internal"), "{label}: {response}");
    }
}

fn limited(addr: SocketAddr, global_ip: u64, anonymous_ip: Option<u64>) -> GatewayOptions {
    let mut options = gate(addr, &["svc"]);
    options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(global_ip, 60.0),
        BucketLimit::new(1_000_000, 60.0),
    );
    options.rate_limit.anonymous_ip_limit = anonymous_ip;
    options.rate_limit.anonymous_ip_ttl = anonymous_ip.map(|_| 60.0);
    options.rate_limit.trust_proxy = TrustProxy::All;
    options.redis = rate_limit_common::redis_options();
    options
}

/// WS-18: an upgrade counts against the Global IP Bucket (keyed on the client address the
/// `trust proxy` setting gives, as for HTTP requests) before any middleware runs; a full
/// Bucket answers 429 and nothing is forwarded.
#[tokio::test]
async fn d15_upgrades_count_against_the_global_ip_bucket() {
    require_redis!();
    let upstream = EchoUpstream::start().await;
    let middleware = Counting::default();
    let served = serve(
        Gateway::builder(limited(upstream.addr, 2, None)).websocket_middleware(middleware.clone()),
        true,
    )
    .await;
    let client = format!("x-forwarded-for: client-{}\r\n", nonce());
    let mut tunnels = Vec::new();
    for _ in 0..2 {
        let mut stream = TcpStream::connect(served.addr).await.expect("connect");
        stream
            .write_all(&upgrade_request("/svc/socket", &client))
            .await
            .expect("send");
        let head = read_head(&mut stream).await;
        assert!(
            head.starts_with(b"HTTP/1.1 101 "),
            "{}",
            String::from_utf8_lossy(&head)
        );
        tunnels.push(stream);
    }
    let response = exchange(served.addr, &upgrade_request("/svc/socket", &client)).await;
    assert_eq!(
        status_and_body(&response).0,
        "HTTP/1.1 429 Too Many Requests"
    );
    assert_eq!(upstream.accepted(), 2);
    assert_eq!(
        middleware.calls.load(Ordering::SeqCst),
        2,
        "no middleware ran for the 429"
    );

    // Another client address has its own Bucket.
    let other = format!("x-forwarded-for: client-{}\r\n", nonce());
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(&upgrade_request("/svc/socket", &other))
        .await
        .expect("send");
    assert!(read_head(&mut stream).await.starts_with(b"HTTP/1.1 101 "));
}

/// WS-18: an upgrade the WebSocket middleware rejects counts against the Anonymous IP
/// Bucket, as a rejected Routed Request does (RL-10); once it is full the client gets 429.
#[tokio::test]
async fn d15_rejected_upgrades_count_against_the_anonymous_ip_bucket() {
    require_redis!();
    let upstream = EchoUpstream::start().await;
    let served = serve(
        Gateway::builder(limited(upstream.addr, 1_000_000, Some(1)))
            .websocket_middleware(Counting::default()),
        true,
    )
    .await;
    let denied = format!("x-forwarded-for: client-{}\r\nx-ws: deny\r\n", nonce());
    let first = exchange(served.addr, &upgrade_request("/svc/socket", &denied)).await;
    assert_eq!(
        status_and_body(&first),
        ("HTTP/1.1 403 Forbidden", FORBIDDEN)
    );
    let second = exchange(served.addr, &upgrade_request("/svc/socket", &denied)).await;
    assert_eq!(status_and_body(&second).0, "HTTP/1.1 429 Too Many Requests");
    assert_eq!(upstream.accepted(), 0);
}
