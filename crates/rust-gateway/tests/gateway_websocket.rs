//! Rust-only tests of WebSocket proxying (area 06): Deviation D1 (WS-05), the order of the
//! middleware and the WS-06 check, and tunnels on shutdown.

mod gateway_common;
mod websocket_common;

use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{GatewayRequest, MiddlewareError, WsProxyMiddleware, async_trait};
use api_gateway::request::ProxyRequest;
use api_gateway::upstream::UpstreamService;
use gateway_common::{options, serve};
use http::StatusCode;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpStream;
use websocket_common::{EchoUpstream, read_head, read_to_close, upgrade_request};

/// Behaves according to `x-ws`: `deny`, `throw` (401 with a JSON body) or `crash`.
struct Scripted;

#[async_trait]
impl WsProxyMiddleware for Scripted {
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "u-1");
        match request.header("x-ws") {
            Some("deny") => Ok(false),
            Some("throw") => Err(MiddlewareError::json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({ "message": "token expired" }),
            )),
            Some("crash") => Err(MiddlewareError::other("boom")),
            _ => Ok(true),
        }
    }
}

fn service(upstream: &EchoUpstream) -> UpstreamService {
    UpstreamService::new(
        "svc",
        format!("http://{}/openapi.json", upstream.addr),
        format!("http://{}", upstream.addr),
    )
}

async fn send(addr: std::net::SocketAddr, request: &[u8]) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(request).await.expect("send");
    stream
}

/// Splits a response into its head (status line and headers, lower-cased names) and body.
fn split(response: &[u8]) -> (String, Vec<String>, String) {
    let text = String::from_utf8_lossy(response).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a complete response");
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").unwrap_or((line, ""));
            format!("{}: {value}", name.to_ascii_lowercase())
        })
        .collect();
    (status, headers, body.to_string())
}

/// D1 (WS-05): a rejected upgrade is answered with the HTTP path's error and the connection
/// is closed; the reference leaves it open with no answer. The middleware run before the
/// WS-06 check, so a rejected `POST` upgrade is answered too.
#[tokio::test]
async fn d1_rejected_upgrades_are_answered_and_closed() {
    let upstream = EchoUpstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![service(&upstream)])).websocket_middleware(Scripted),
        true,
    )
    .await;
    let post = {
        let mut request = upgrade_request("/svc/socket", "x-ws: deny\r\n");
        request.splice(0..3, b"POST".iter().copied());
        request
    };
    let cases: [(&str, Vec<u8>, &str, &str); 5] = [
        (
            "middleware denies",
            upgrade_request("/svc/socket", "x-ws: deny\r\n"),
            "HTTP/1.1 403 Forbidden",
            r#"{"message":"Forbidden","statusCode":403}"#,
        ),
        (
            "middleware throws with a status",
            upgrade_request("/svc/socket", "x-ws: throw\r\n"),
            "HTTP/1.1 401 Unauthorized",
            r#"{"message":"token expired"}"#,
        ),
        (
            "middleware crashes",
            upgrade_request("/svc/socket", "x-ws: crash\r\n"),
            "HTTP/1.1 500 Internal Server Error",
            r#"{"statusCode":500,"message":"Internal server error"}"#,
        ),
        (
            "no Upstream Service",
            upgrade_request("/nowhere/socket", ""),
            "HTTP/1.1 503 Service Unavailable",
            r#"{"message":"Service Unavailable","statusCode":503}"#,
        ),
        (
            "middleware denies a POST upgrade",
            post,
            "HTTP/1.1 403 Forbidden",
            r#"{"message":"Forbidden","statusCode":403}"#,
        ),
    ];
    for (label, request, status, body) in cases {
        let mut stream = send(served.addr, &request).await;
        let response = read_to_close(&mut stream).await;
        let (seen_status, headers, seen_body) = split(&response);
        assert_eq!(seen_status, status, "{label}");
        assert_eq!(seen_body, body, "{label}");
        assert!(
            headers.contains(&"content-type: application/json; charset=utf-8".to_string()),
            "{label}: {headers:?}"
        );
        assert!(
            headers.contains(&format!("content-length: {}", body.len())),
            "{label}: {headers:?}"
        );
        assert!(
            headers.contains(&"connection: close".to_string()),
            "{label}: {headers:?}"
        );
    }
    assert_eq!(upstream.accepted(), 0, "nothing was forwarded");
}

/// WS-06 with a middleware that allows: the connection is dropped with no response.
#[tokio::test]
async fn not_a_websocket_is_dropped_without_a_response() {
    let upstream = EchoUpstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![service(&upstream)])).websocket_middleware(Scripted),
        true,
    )
    .await;
    let mut h2c = upgrade_request("/svc/socket", "");
    let at = h2c
        .windows(9)
        .position(|w| w == b"websocket")
        .expect("upgrade value");
    h2c.splice(at..at + 9, b"h2c".iter().copied());
    let mut stream = send(served.addr, &h2c).await;
    assert!(read_to_close(&mut stream).await.is_empty());
    assert_eq!(upstream.accepted(), 0);
}

/// Stopping the server closes open tunnels, so that `serve` returns.
#[tokio::test]
async fn shutdown_closes_open_tunnels() {
    let upstream = EchoUpstream::start().await;
    let served = serve(Gateway::builder(options(vec![service(&upstream)])), true).await;
    let mut stream = send(served.addr, &upstream_request()).await;
    let head = read_head(&mut stream).await;
    assert!(head.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"));
    tokio::time::timeout(Duration::from_secs(10), served.shutdown())
        .await
        .expect("serve returns while a tunnel is open")
        .expect("serve");
    assert!(read_to_close(&mut stream).await.is_empty());
    assert_eq!(
        upstream.wait_open_at_most(0, Duration::from_secs(5)).await,
        0
    );
}

fn upstream_request() -> Vec<u8> {
    upgrade_request("/svc/socket", "")
}

/// The router served without [`Gateway::serve`] cannot proxy upgrades: it answers 501.
#[tokio::test]
async fn the_router_alone_answers_upgrades_with_501() {
    let upstream = EchoUpstream::start().await;
    let gateway = Gateway::builder(options(vec![service(&upstream)]))
        .build()
        .expect("gateway builds");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("address");
    let server = tokio::spawn(api_gateway::server::serve(
        gateway.router(),
        listener,
        std::future::pending(),
    ));
    gateway.ready().await;
    let mut stream = send(addr, &upgrade_request("/svc/socket", "")).await;
    let head = read_head(&mut stream).await;
    assert!(
        head.starts_with(b"HTTP/1.1 501 Not Implemented\r\n"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    assert_eq!(upstream.accepted(), 0);
    server.abort();
}
