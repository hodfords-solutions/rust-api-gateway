//! Security tests: WebSocket upgrades (`docs/security/attack-tests.md`).
//!
//! `svc` is an echoing WebSocket upstream ([`EchoUpstream`]); `default`, the Default Service,
//! is a plain HTTP upstream ([`Upstream`]) that ignores `Upgrade` and answers every request
//! with 200, as most HTTP frameworks do. Its document's Routes are `/v1/users` and
//! `/v1/limited`. A proxy middleware stands in for the application's authentication: it
//! rejects every Routed Request.

mod gateway_common;
mod websocket_common;

use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{
    GatewayRequest, MiddlewareError, ProxyMiddleware, WsProxyMiddleware, async_trait,
};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use api_gateway::upstream::UpstreamService;
use gateway_common::{Served, Sink, Upstream, options, parse_responses, raw_send, serve};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use websocket_common::{EchoUpstream, read_head, upgrade_request};

/// The application's authentication: no Routed Request is allowed.
struct DenyAll;

#[async_trait]
impl ProxyMiddleware for DenyAll {
    async fn handle(
        &self,
        _route: &Route,
        _request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        Ok(false)
    }
}

/// A WebSocket middleware that only admits upgrades from `https://app.example`.
struct SameOrigin;

#[async_trait]
impl WsProxyMiddleware for SameOrigin {
    async fn handle(
        &self,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        Ok(request.header("origin") == Some("https://app.example"))
    }
}

struct Setup {
    echo: EchoUpstream,
    http: Upstream,
    served: Served,
}

impl Setup {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    async fn start_with(check_origin: bool) -> Self {
        let echo = EchoUpstream::start().await;
        let http = Upstream::start().await;
        let svc = UpstreamService::new(
            "svc",
            format!("http://{}/openapi.json", echo.addr),
            format!("http://{}", echo.addr),
        );
        let mut builder =
            Gateway::builder(options(vec![svc, http.service("default")])).proxy_middleware(DenyAll);
        if check_origin {
            builder = builder.websocket_middleware(SameOrigin);
        }
        let served = serve(builder, true).await;
        Self { echo, http, served }
    }
}

/// The precondition of SEC-01: the middleware rejects a plain request.
#[tokio::test]
async fn plain_requests_are_rejected_by_the_middleware() {
    let setup = Setup::start().await;
    let raw = raw_send(
        setup.served.addr,
        b"GET /v1/users HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(parse_responses(&raw).first().map(|r| r.status), Some(403));
    assert!(setup.http.seen().is_empty());
}

/// SEC-01, fixed (Deviation D15): adding `Connection: Upgrade` and `Upgrade: websocket` to
/// a `GET` used to send it down the WebSocket path, which had no Route lookup, no proxy
/// middleware and no rate limiting (WS-01, WS-04), and an upstream that ignored the upgrade
/// had its answer relayed (WS-15): the application's authentication was bypassed. Now an
/// upgrade whose URL is a Route passes that Route's checks: the proxy middleware rejects it
/// like the plain request, with 403, and nothing reaches the upstream.
#[tokio::test]
async fn sec_01_upgrade_requests_cannot_bypass_the_proxy_middleware() {
    let setup = Setup::start().await;
    let raw = raw_send(
        setup.served.addr,
        &upgrade_request("/v1/users", ""),
        Duration::from_secs(2),
    )
    .await;
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{text:?}");
    assert!(
        !text.contains("upstream /v1/users"),
        "the upstream's answer was relayed: {text:?}"
    );
    assert!(setup.http.seen().is_empty(), "{:?}", setup.http.seen());
}

/// SEC-01, fixed (Deviation D15): an upgrade on a path that is no Route still reaches the
/// upstream (WebSocket endpoints are rarely in OpenAPI documents; WebSocket middleware are
/// their check), but when the upstream does not upgrade, the client gets its status line
/// only: whatever an unpublished endpoint answers is not passed on.
#[tokio::test]
async fn sec_01_a_refused_upgrade_passes_on_no_upstream_content() {
    let setup = Setup::start().await;
    let raw = raw_send(
        setup.served.addr,
        &upgrade_request("/internal/admin", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&raw), "HTTP/1.1 200 OK\r\n\r\n");
    assert_eq!(setup.http.seen().len(), 1, "the upstream was asked");
}

/// Legitimate WebSocket traffic is unchanged: an endpoint that is no Route is tunnelled with
/// the WebSocket middleware only, even though the proxy middleware rejects everything.
#[tokio::test]
async fn websocket_endpoints_without_a_route_still_upgrade() {
    let setup = Setup::start().await;
    let mut stream = TcpStream::connect(setup.served.addr)
        .await
        .expect("connect");
    stream
        .write_all(&upgrade_request("/svc/socket", ""))
        .await
        .expect("send");
    assert!(read_head(&mut stream).await.starts_with(b"HTTP/1.1 101"));
    stream.write_all(b"ping").await.expect("write");
    let mut back = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut back))
        .await
        .expect("echo in time")
        .expect("read");
    assert_eq!(&back, b"ping");
}

/// Accepts a request carrying `x-token: ok` and tells the upstream who it is
/// (`authUserId`, forwarded as `auth-user-id`, an excluded header).
struct TokenAuth;

#[async_trait]
impl ProxyMiddleware for TokenAuth {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "u-1");
        Ok(request.header("x-token") == Some("ok"))
    }
}

/// Deviation D15: an upgrade on a Route that the proxy middleware admits is forwarded with
/// the headers the middleware set, the excluded identity header included (and a client's
/// own copy removed), as the plain request would be.
#[tokio::test]
async fn an_admitted_upgrade_on_a_route_carries_the_middleware_headers() {
    let http = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![http.service("default")])).proxy_middleware(TokenAuth),
        true,
    )
    .await;
    let raw = raw_send(
        served.addr,
        &upgrade_request("/v1/users", "Auth-User-Id: admin\r\n"),
        Duration::from_secs(2),
    )
    .await;
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 403 "));
    assert!(http.seen().is_empty());
    let _ = raw_send(
        served.addr,
        &upgrade_request("/v1/users", "X-Token: ok\r\nAuth-User-Id: admin\r\n"),
        Duration::from_secs(2),
    )
    .await;
    let seen = http.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let identity: Vec<&str> = seen[0]
        .headers
        .iter()
        .filter(|(n, _)| n == "auth-user-id")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(identity, ["u-1"]);
}

/// The gateway never reports an upgrade the upstream did not grant, and never opens a
/// tunnel on a refused one: the upstream's non-101 status line is passed on and the
/// connection closed, so an HTTP request pipelined behind the upgrade request (WebSocket smuggling)
/// never reaches the upstream.
#[tokio::test]
async fn refused_upgrades_open_no_tunnel() {
    let setup = Setup::start().await;
    let mut request = upgrade_request("/v1/ws", "");
    request.extend_from_slice(b"GET /v1/smuggled HTTP/1.1\r\nHost: gw\r\n\r\n");
    let raw = raw_send(setup.served.addr, &request, Duration::from_secs(3)).await;
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.starts_with("HTTP/1.1 101"), "{text:?}");
    assert!(
        setup
            .http
            .seen()
            .iter()
            .all(|s| !s.target.contains("smuggled")),
        "smuggled: {:?}",
        setup.http.seen()
    );
}

/// Upgrades that are not WebSocket (`h2c`, the classic h2c-smuggling vector) or not `GET`
/// are dropped without a response and without contacting an upstream (WS-06).
#[tokio::test]
async fn non_websocket_upgrades_are_dropped() {
    let setup = Setup::start().await;
    for request in [
        b"GET /svc/x HTTP/1.1\r\nHost: gw\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\nHTTP2-Settings: AAMAAABkAARAAAAAAAIAAAAA\r\n\r\n".to_vec(),
        b"POST /svc/x HTTP/1.1\r\nHost: gw\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nContent-Length: 0\r\n\r\n".to_vec(),
    ] {
        let raw = raw_send(setup.served.addr, &request, Duration::from_secs(2)).await;
        assert!(raw.is_empty(), "{:?}", String::from_utf8_lossy(&raw));
    }
    assert_eq!(setup.echo.accepted(), 0);
    assert!(setup.http.seen().is_empty());
}

/// SSRF over the WebSocket path: an absolute-form target and a `Host` naming another address
/// select nothing; the upgrade goes to the configured service host.
#[tokio::test]
async fn upgrade_targets_cannot_choose_the_upstream_host() {
    let setup = Setup::start().await;
    let sink = Sink::start().await;
    let at = format!("127.0.0.1:{}", sink.addr.port());
    let mut stream = TcpStream::connect(setup.served.addr)
        .await
        .expect("connect");
    let request = format!(
        "GET http://{at}/svc/x HTTP/1.1\r\nHost: {at}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let head = read_head(&mut stream).await;
    assert!(head.starts_with(b"HTTP/1.1 101"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sink.accepted(),
        0,
        "the gateway connected to the client's host"
    );
    assert_eq!(setup.echo.accepted(), 1);
}

/// The handshake headers reach the upstream unchanged (no key or version is added, and
/// `Origin` is kept), so the upstream can validate the handshake and the origin; a client's
/// copy of the excluded identity header is removed (WS-02).
#[tokio::test]
async fn handshake_headers_reach_the_upstream_unchanged() {
    let setup = Setup::start().await;
    let request = b"GET /v1/ws HTTP/1.1\r\nHost: gw\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 1337\r\nOrigin: https://evil.example\r\nAuth-User-Id: admin\r\n\r\n";
    let _ = raw_send(setup.served.addr, request, Duration::from_secs(2)).await;
    let seen = setup.http.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let get = |name: &str| -> Vec<&str> {
        seen[0]
            .headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    assert!(get("sec-websocket-key").is_empty());
    assert_eq!(get("sec-websocket-version"), ["1337"]);
    assert_eq!(get("origin"), ["https://evil.example"]);
    assert!(get("auth-user-id").is_empty());
}

/// Cross-site WebSocket hijacking is left to the application, and it can stop it: a
/// WebSocket middleware that checks `Origin` refuses a foreign origin with 403 before any
/// upstream connection, and admits its own.
#[tokio::test]
async fn a_websocket_middleware_can_enforce_the_origin() {
    let setup = Setup::start_with(true).await;
    let raw = raw_send(
        setup.served.addr,
        &upgrade_request("/svc/x", "Origin: https://evil.example\r\n"),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 403"),
        "{:?}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(setup.echo.accepted(), 0);

    let mut stream = TcpStream::connect(setup.served.addr)
        .await
        .expect("connect");
    stream
        .write_all(&upgrade_request(
            "/svc/x",
            "Origin: https://app.example\r\n",
        ))
        .await
        .expect("send");
    assert!(read_head(&mut stream).await.starts_with(b"HTTP/1.1 101"));
}

/// Oversize frames are the endpoints' business: the tunnel passes bytes through as they
/// arrive, without buffering a frame. A frame announcing a 16 MiB payload starts coming
/// back from the echoing upstream while most of it has not been sent.
#[tokio::test]
async fn large_frames_stream_through_the_tunnel() {
    let setup = Setup::start().await;
    let mut stream = TcpStream::connect(setup.served.addr)
        .await
        .expect("connect");
    stream
        .write_all(&upgrade_request("/svc/x", ""))
        .await
        .expect("send");
    assert!(read_head(&mut stream).await.starts_with(b"HTTP/1.1 101"));
    // A masked binary frame header with a 64-bit length of 16 MiB, then 256 KiB of it.
    let mut frame = vec![0x82, 0xff];
    frame.extend_from_slice(&(16u64 * 1024 * 1024).to_be_bytes());
    frame.extend_from_slice(&[1, 2, 3, 4]);
    frame.extend_from_slice(&vec![b'x'; 256 * 1024]);
    stream.write_all(&frame).await.expect("frame");
    let mut back = vec![0u8; frame.len()];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut back))
        .await
        .expect("echoed while the frame is incomplete")
        .expect("read");
    assert_eq!(back, frame);
}

/// Deviation D15 follows the request classes: a Bypass Request skips the Route's checks over
/// HTTP, so its upgrade does too (and the upstream's answer is still reduced to its status
/// line).
#[tokio::test]
async fn a_bypass_upgrade_skips_the_route_checks_like_the_plain_request() {
    let http = Upstream::start().await;
    let mut options = options(vec![http.service("default")]);
    options.bypass_route_prefixes = vec!["v1".into()];
    let served = serve(Gateway::builder(options).proxy_middleware(DenyAll), true).await;
    let plain = raw_send(
        served.addr,
        b"GET /v1/users HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(parse_responses(&plain).first().map(|r| r.status), Some(200));
    let raw = raw_send(
        served.addr,
        &upgrade_request("/v1/users", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&raw), "HTTP/1.1 200 OK\r\n\r\n");
    assert_eq!(http.seen().len(), 2);
}
