//! Area 06: WebSocket proxying (WS-01 to WS-16).
//!
//! The path gate, the IP rate limits and the 502 for an upstream that does not switch
//! protocols (WS-15, WS-17, WS-18) differ from the pinned reference by design (Deviation D15);
//! their reference side is in `deviations::d15_*`, the Rust side is Rust-only. Scenarios here
//! set `websocketRoutePrefixes` to every path they upgrade on.
//!
//! Rejected upgrades (WS-05) differ by design (Deviation D1): the reference leaves the
//! connection open with no answer. Scenarios here therefore never compare what the client
//! sees for a rejected upgrade; they compare what reaches the upstream. The reference side
//! of D1 is in `deviations::d1_rejected_upgrade_hangs`, the Rust side is Rust-only.
//!
//! The reference process crashes (unhandled EPIPE) when a client drops a tunnel the gateway
//! is still writing to, so no scenario closes a client while data flows towards it.

use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use conformance::client::{self, RawConn, ReadOutcome, Request};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::{Behaviour, MockUpstream, RecordedRequest};
use conformance::runner::{Gateway, Observations, run};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use crate::common::{assert_table, users_gateway};
use crate::pipeline::mask_port;

/// How long a scenario waits to be sure something did not happen.
const QUIET: Duration = Duration::from_millis(1500);
/// How long a scenario waits for something that must happen.
const WAIT: Duration = Duration::from_secs(10);

/// What the client saw after sending an upgrade request.
enum Answer {
    /// A response head, exactly as received, and the open connection after it.
    Head(Vec<u8>, RawConn),
    /// The connection closed with no byte received.
    Closed,
    /// Nothing within [`QUIET`]; the connection is still open.
    Silent,
}

impl Answer {
    fn status(&self) -> String {
        match self {
            Answer::Head(head, _) => status_line(head),
            Answer::Closed => "closed, no response".into(),
            Answer::Silent => "no answer".into(),
        }
    }
}

fn status_line(head: &[u8]) -> String {
    let text = String::from_utf8_lossy(head);
    text.split("\r\n").next().unwrap_or_default().to_string()
}

/// Sends `request` on a new connection and reports what came back.
async fn send_upgrade(gw: &Gateway, request: &Request) -> Result<Answer> {
    let mut conn = RawConn::connect(gw.addr()).await?;
    conn.send(request).await?;
    answer(conn).await
}

async fn answer(mut conn: RawConn) -> Result<Answer> {
    match conn.read_some(QUIET).await {
        ReadOutcome::Closed => Ok(Answer::Closed),
        ReadOutcome::Timeout => Ok(Answer::Silent),
        ReadOutcome::Data(first) => {
            // Put the bytes back in front of what follows and read the head.
            let (stream, mut leftover) = conn.into_parts();
            let mut bytes = first;
            bytes.append(&mut leftover);
            let mut conn = RawConn::from_parts(stream, bytes);
            let head = conn
                .read_head_raw(WAIT)
                .await?
                .context("connection closed inside a response head")?;
            Ok(Answer::Head(head, conn))
        }
    }
}

/// Sends an upgrade request that must succeed and returns the client WebSocket.
async fn open(gw: &Gateway, request: &Request) -> Result<WebSocketStream<tokio::net::TcpStream>> {
    match send_upgrade(gw, request).await? {
        Answer::Head(head, conn) if status_line(&head).starts_with("HTTP/1.1 101 ") => {
            Ok(conn.into_websocket().await)
        }
        other => bail!("{}: expected 101, got {}", request.target, other.status()),
    }
}

/// Sends `message` and returns the next data message.
async fn round_trip(
    ws: &mut WebSocketStream<tokio::net::TcpStream>,
    message: Message,
) -> Result<Message> {
    ws.send(message).await?;
    next_data(ws).await
}

async fn next_data(ws: &mut WebSocketStream<tokio::net::TcpStream>) -> Result<Message> {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .context("no WebSocket message")?
            .context("WebSocket closed")??;
        if matches!(message, Message::Text(_) | Message::Binary(_)) {
            return Ok(message);
        }
    }
}

/// Header lines as the upstream received them, names in their original case, the `host`
/// port masked.
fn raw_header_lines(request: &RecordedRequest) -> Vec<String> {
    request
        .headers
        .iter()
        .map(|(n, v)| {
            let v = if n.eq_ignore_ascii_case("host") {
                mask_port(v)
            } else {
                v.clone()
            };
            format!("{n}: {v}")
        })
        .collect()
}

/// A masked client-to-server text frame.
fn masked_text_frame(text: &str) -> Vec<u8> {
    let payload = text.as_bytes();
    assert!(payload.len() < 126);
    let mask = [0x12u8, 0x34, 0x56, 0x78];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    frame
}

/// A masked client-to-server Close frame without a payload.
fn masked_close_frame() -> Vec<u8> {
    vec![0x88, 0x80, 0x12, 0x34, 0x56, 0x78]
}

/// Waits until the mock recorded `count` requests, or [`QUIET`] passed; returns them.
async fn requests_after_quiet(mock: &MockUpstream) -> Vec<RecordedRequest> {
    tokio::time::sleep(QUIET).await;
    mock.requests()
}

/// WS-01: every upgrade request is handled by the WebSocket path, whatever its path:
/// paths with a Route, without one, Bypass and Static Requests, the application's own
/// route, the gateway's endpoints, and a path Express would reject. A request is an upgrade
/// when it has an `Upgrade` header and `Connection` lists `upgrade`; with `Upgrade` alone it
/// is an ordinary request. Under Deviation D15 only paths under `websocketRoutePrefixes`
/// are upgraded, so every path here is listed.
#[tokio::test]
async fn ws_01_every_upgrade_takes_the_websocket_path() {
    run(
        "ws_01_every_upgrade_takes_the_websocket_path",
        &["WS-01"],
        |target| async move {
            let users = MockUpstream::start().await?;
            users.set_document_json(fixtures::USERS_SERVICE);
            users.set_default(Behaviour::WebSocketEcho);
            let fallback = MockUpstream::start().await?;
            fallback.set_document_json(fixtures::DEFAULT_SERVICE);
            fallback.set_default(Behaviour::WebSocketEcho);
            let mut config = GatewayConfig::new()
                .service("svc", &users)
                .service("default", &fallback);
            config.bypass_route_prefixes = Some(vec!["svc/public".into()]);
            config.websocket_route_prefixes = Some(
                ["svc", "__conformance", "documents", "document-json", "mcp"]
                    .map(String::from)
                    .to_vec(),
            );
            let gw = target.start(config).await?;

            let cases = [
                ("Route", client::upgrade_request("/svc/v1/users")),
                ("no Route", client::upgrade_request("/svc/no/such/route")),
                ("Bypass", client::upgrade_request("/svc/public/x")),
                (
                    "Static",
                    client::upgrade_request("/svc/v1/users").header("x-test-static", "1"),
                ),
                ("malformed %", client::upgrade_request("/svc/%zz")),
                (
                    "application route",
                    client::upgrade_request("/__conformance/ready"),
                ),
                ("documents endpoint", client::upgrade_request("/documents")),
                (
                    "document-json endpoint",
                    client::upgrade_request("/document-json"),
                ),
                ("mcp endpoint", client::upgrade_request("/mcp")),
                (
                    "connection token list",
                    Request::get("/svc/socket")
                        .header("Connection", "keep-alive, Upgrade")
                        .header("Upgrade", "websocket")
                        .header("Sec-WebSocket-Version", "13")
                        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ),
                (
                    "lower-case connection token",
                    Request::get("/svc/socket")
                        .header("connection", "upgrade")
                        .header("upgrade", "websocket")
                        .header("Sec-WebSocket-Version", "13")
                        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ),
            ];
            let mut observed = Vec::new();
            for (label, request) in cases {
                users.clear_requests();
                fallback.clear_requests();
                let answer = send_upgrade(&gw, &request).await?;
                let reached = match (users.last_request(), fallback.last_request()) {
                    (Some(r), None) => format!("svc {}", r.target),
                    (None, Some(r)) => format!("default {}", r.target),
                    (None, None) => "nothing".into(),
                    _ => "both".into(),
                };
                observed.push((
                    label.to_string(),
                    format!("{} -> {reached}", answer.status()),
                ));
            }

            // `Upgrade` without `Connection: upgrade` is an ordinary Routed Request.
            users.set_default(Behaviour::Echo);
            users.clear_requests();
            let plain = gw
                .send(&Request::get("/svc/v1/users").header("Upgrade", "websocket"))
                .await?;
            let forwarded = users.last_request().context("plain request forwarded")?;
            observed.push((
                "Upgrade without Connection: upgrade".into(),
                format!(
                    "{} -> svc {} upgrade header {:?}",
                    plain.status,
                    forwarded.target,
                    forwarded.header("upgrade")
                ),
            ));

            assert_table(
                "WS-01",
                &observed,
                &[
                    (
                        "Route",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/v1/users",
                    ),
                    (
                        "no Route",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/no/such/route",
                    ),
                    (
                        "Bypass",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/public/x",
                    ),
                    (
                        "Static",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/v1/users",
                    ),
                    (
                        "malformed %",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/%zz",
                    ),
                    (
                        "application route",
                        "HTTP/1.1 101 Switching Protocols -> default /__conformance/ready",
                    ),
                    (
                        "documents endpoint",
                        "HTTP/1.1 101 Switching Protocols -> default /documents",
                    ),
                    (
                        "document-json endpoint",
                        "HTTP/1.1 101 Switching Protocols -> default /document-json",
                    ),
                    (
                        "mcp endpoint",
                        "HTTP/1.1 101 Switching Protocols -> default /mcp",
                    ),
                    (
                        "connection token list",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/socket",
                    ),
                    (
                        "lower-case connection token",
                        "HTTP/1.1 101 Switching Protocols -> svc /svc/socket",
                    ),
                    (
                        "Upgrade without Connection: upgrade",
                        "200 -> svc /v1/users/ upgrade header None",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("cases", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// WS-02: the URL is normalised and the `excludeHeaders` are removed before anything else.
#[tokio::test]
async fn ws_02_sanitising() {
    run("ws_02_sanitising", &["WS-02"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut observed = Vec::new();
        for target in [
            "/svc/a/../socket?x=1",
            "/svc/./b/%2E%2E/socket",
            "/svc\\socket",
            "/svc/socket#fragment",
            "/svc/a^b",
        ] {
            mock.clear_requests();
            let request = client::upgrade_request(target)
                .header("Auth-User-Id", "spoofed")
                .header("permission-in-any-guard", "admin");
            let answer = send_upgrade(&gw, &request).await?;
            let recorded = mock.last_request();
            let excluded = recorded.as_ref().map(|r| {
                r.header("auth-user-id").is_some() || r.header("permission-in-any-guard").is_some()
            });
            observed.push((
                target.to_string(),
                format!(
                    "{} -> {} excluded headers forwarded: {excluded:?}",
                    answer.status(),
                    recorded.map(|r| r.target).unwrap_or_default()
                ),
            ));
        }
        assert_table(
            "WS-02",
            &observed,
            &[
                (
                    "/svc/a/../socket?x=1",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket?x=1 excluded headers forwarded: Some(false)",
                ),
                (
                    "/svc/./b/%2E%2E/socket",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket excluded headers forwarded: Some(false)",
                ),
                (
                    "/svc\\socket",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket excluded headers forwarded: Some(false)",
                ),
                (
                    "/svc/socket#fragment",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket excluded headers forwarded: Some(false)",
                ),
                (
                    "/svc/a^b",
                    "HTTP/1.1 101 Switching Protocols -> /svc/a%5Eb excluded headers forwarded: Some(false)",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("cases", &observed);
        Ok(obs)
    })
    .await;
}

/// WS-03: the Upstream Service is selected as for HTTP requests: Direct Prefixes, then
/// Service Prefixes, then the Default Service for an unmatched prefix or a service whose
/// document is not loaded.
#[tokio::test]
async fn ws_03_service_selection() {
    run("ws_03_service_selection", &["WS-03"], |target| async move {
        let svc = MockUpstream::start().await?;
        svc.set_document_json(fixtures::USERS_SERVICE);
        let other = MockUpstream::start().await?;
        other.set_document_json(fixtures::DEFAULT_SERVICE);
        let down = MockUpstream::start().await?;
        let fallback = MockUpstream::start().await?;
        fallback.set_document_json(fixtures::DEFAULT_SERVICE);
        let mocks = [
            ("svc", &svc),
            ("other", &other),
            ("down", &down),
            ("default", &fallback),
        ];
        for (_, mock) in mocks {
            mock.set_default(Behaviour::WebSocketEcho);
        }
        let gw = target
            .start(
                GatewayConfig::new()
                    .service_with_direct("svc", &svc, &["oauth"])
                    .service("other", &other)
                    .service("down", &down)
                    .service("default", &fallback),
            )
            .await?;
        let mut observed = Vec::new();
        for path in [
            "/oauth/socket",
            "/svc/socket",
            "/other/socket",
            "/down/socket",
            "/nowhere/socket",
        ] {
            for (_, mock) in mocks {
                mock.clear_requests();
            }
            let answer = send_upgrade(&gw, &client::upgrade_request(path)).await?;
            let reached: Vec<String> = mocks
                .iter()
                .filter_map(|(name, mock)| {
                    mock.last_request().map(|r| format!("{name} {}", r.target))
                })
                .collect();
            observed.push((
                path.to_string(),
                format!("{} -> {}", answer.status(), reached.join(", ")),
            ));
        }
        assert_table(
            "WS-03",
            &observed,
            &[
                (
                    "/oauth/socket",
                    "HTTP/1.1 101 Switching Protocols -> svc /oauth/socket",
                ),
                (
                    "/svc/socket",
                    "HTTP/1.1 101 Switching Protocols -> svc /svc/socket",
                ),
                (
                    "/other/socket",
                    "HTTP/1.1 101 Switching Protocols -> other /other/socket",
                ),
                (
                    "/down/socket",
                    "HTTP/1.1 101 Switching Protocols -> default /down/socket",
                ),
                (
                    "/nowhere/socket",
                    "HTTP/1.1 101 Switching Protocols -> default /nowhere/socket",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("cases", &observed);
        Ok(obs)
    })
    .await;
}

/// WS-04: the WebSocket middleware run for every upgrade, after the `excludeHeaders` were
/// removed; their headers reach the upstream, replacing same-named client headers in place.
/// A middleware that rejects stops the upgrade, also for Static and Bypass paths (there is
/// no Static or Bypass handling) and for paths with no Route (there is no Route lookup).
#[tokio::test]
async fn ws_04_middleware() {
    run("ws_04_middleware", &["WS-04"], |target| async move {
        let (mock, gw) = users_gateway(&target, |mut c| {
            c.bypass_route_prefixes = Some(vec!["svc/public".into()]);
            c
        })
        .await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut obs = Observations::new();

        mock.clear_requests();
        let request = client::upgrade_request("/svc/socket")
            .header("Auth-User-Id", "spoofed")
            .header("x-keep", "original")
            .header("x-test-auth-user", "u-1")
            .header("x-test-add-headers", r#"{"xKeep":"override","newOne":"n"}"#);
        let answer = send_upgrade(&gw, &request).await?;
        ensure!(
            answer.status().starts_with("HTTP/1.1 101 "),
            "allowed: {}",
            answer.status()
        );
        let recorded = mock.last_request().context("upgrade forwarded")?;
        let lines: Vec<String> = raw_header_lines(&recorded)
            .into_iter()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.starts_with("auth-user-id:")
                    || l.starts_with("x-keep:")
                    || l.starts_with("new-one:")
            })
            .collect();
        assert_eq!(
            lines,
            ["x-keep: override", "auth-user-id: u-1", "new-one: n"]
        );
        obs.record("middleware headers upstream", &lines);

        let mut rejected = Vec::new();
        for (label, request) in [
            (
                "deny",
                client::upgrade_request("/svc/socket").header("x-test-middleware", "deny"),
            ),
            (
                "throw",
                client::upgrade_request("/svc/socket").header("x-test-middleware", "throw"),
            ),
            (
                "crash",
                client::upgrade_request("/svc/socket").header("x-test-middleware", "crash"),
            ),
            (
                "deny on a Static Request",
                client::upgrade_request("/svc/v1/users")
                    .header("x-test-static", "1")
                    .header("x-test-middleware", "deny"),
            ),
            (
                "deny on a Bypass Request",
                client::upgrade_request("/svc/public/x").header("x-test-middleware", "deny"),
            ),
            (
                "deny on a path with no Route",
                client::upgrade_request("/svc/no/route").header("x-test-middleware", "deny"),
            ),
        ] {
            mock.clear_requests();
            let mut conn = RawConn::connect(gw.addr()).await?;
            conn.send(&request).await?;
            let forwarded = requests_after_quiet(&mock).await.len();
            drop(conn);
            rejected.push((
                label.to_string(),
                format!("forwarded upstream: {forwarded}"),
            ));
        }
        assert_table(
            "WS-04 rejections",
            &rejected,
            &[
                ("deny", "forwarded upstream: 0"),
                ("throw", "forwarded upstream: 0"),
                ("crash", "forwarded upstream: 0"),
                ("deny on a Static Request", "forwarded upstream: 0"),
                ("deny on a Bypass Request", "forwarded upstream: 0"),
                ("deny on a path with no Route", "forwarded upstream: 0"),
            ],
        );
        obs.record("rejections", &rejected);
        Ok(obs)
    })
    .await;
}

/// WS-06: a request that is not `GET`, or whose `Upgrade` is not `websocket` in any letter
/// case, has its connection closed with no response and is not forwarded.
#[tokio::test]
async fn ws_06_not_a_websocket() {
    run("ws_06_not_a_websocket", &["WS-06"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut observed = Vec::new();
        let mut post = client::upgrade_request("/svc/socket");
        post.method = "POST".into();
        let mut put = client::upgrade_request("/svc/socket");
        put.method = "PUT".into();
        let h2c = Request::get("/svc/socket")
            .header("Connection", "Upgrade")
            .header("Upgrade", "h2c");
        let mixed_case = Request::get("/svc/socket")
            .header("Connection", "Upgrade")
            .header("Upgrade", "WebSocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==");
        for (label, request) in [
            ("POST", post),
            ("PUT", put),
            ("Upgrade: h2c", h2c),
            ("Upgrade: WebSocket", mixed_case),
        ] {
            mock.clear_requests();
            let answer = send_upgrade(&gw, &request).await?;
            let forwarded = requests_after_quiet(&mock).await.len();
            observed.push((
                label.to_string(),
                format!("{}, forwarded: {forwarded}", answer.status()),
            ));
        }
        assert_table(
            "WS-06",
            &observed,
            &[
                ("POST", "closed, no response, forwarded: 0"),
                ("PUT", "closed, no response, forwarded: 0"),
                ("Upgrade: h2c", "closed, no response, forwarded: 0"),
                (
                    "Upgrade: WebSocket",
                    "HTTP/1.1 101 Switching Protocols, forwarded: 1",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("cases", &observed);
        Ok(obs)
    })
    .await;
}

/// WS-07: the upstream path is the whole normalised URL: Service Prefix kept, query as
/// received, no slash added.
#[tokio::test]
async fn ws_07_upstream_path() {
    run("ws_07_upstream_path", &["WS-07"], |target| async move {
        let svc = MockUpstream::start().await?;
        svc.set_document_json(fixtures::USERS_SERVICE);
        svc.set_default(Behaviour::WebSocketEcho);
        let gw = target
            .start(GatewayConfig::new().service_with_direct("svc", &svc, &["oauth"]))
            .await?;
        let mut observed = Vec::new();
        for path in [
            "/svc",
            "/svc/",
            "/svc?x=1",
            "/svc/socket.io/?EIO=4&transport=websocket",
            "/svc/socket?a=1&b&c=%20d&e=f+g&a=2",
            "/oauth/socket",
        ] {
            svc.clear_requests();
            let answer = send_upgrade(&gw, &client::upgrade_request(path)).await?;
            let upstream = svc.last_request().map(|r| r.target).unwrap_or_default();
            observed.push((
                path.to_string(),
                format!("{} -> {upstream}", answer.status()),
            ));
        }
        assert_table(
            "WS-07",
            &observed,
            &[
                ("/svc", "HTTP/1.1 101 Switching Protocols -> /svc"),
                ("/svc/", "HTTP/1.1 101 Switching Protocols -> /svc/"),
                ("/svc?x=1", "HTTP/1.1 101 Switching Protocols -> /svc?x=1"),
                (
                    "/svc/socket.io/?EIO=4&transport=websocket",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket.io/?EIO=4&transport=websocket",
                ),
                (
                    "/svc/socket?a=1&b&c=%20d&e=f+g&a=2",
                    "HTTP/1.1 101 Switching Protocols -> /svc/socket?a=1&b&c=%20d&e=f+g&a=2",
                ),
                (
                    "/oauth/socket",
                    "HTTP/1.1 101 Switching Protocols -> /oauth/socket",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("cases", &observed);
        Ok(obs)
    })
    .await;
}

/// WS-08: every client header reaches the upstream, hop-by-hop ones included, repeated
/// lines joined as Node joins them, middleware headers replacing same-named ones in place;
/// no `x-forwarded-for` is added and `Host` is unchanged.
#[tokio::test]
async fn ws_08_upstream_headers() {
    run("ws_08_upstream_headers", &["WS-08"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut obs = Observations::new();
        for (label, request) in [
            ("plain", client::upgrade_request("/svc/socket")),
            (
                "many headers",
                client::upgrade_request("/svc/socket")
                    .header("X-Multi", "one")
                    .header("x-multi", "two")
                    .header("Cookie", "a=1")
                    .header("Cookie", "b=2")
                    .header("User-Agent", "first")
                    .header("User-Agent", "second")
                    .header("Keep-Alive", "timeout=5")
                    .header("TE", "trailers")
                    .header("Proxy-Authorization", "Basic eDp5")
                    .header("X-Forwarded-For", "10.0.0.1")
                    .header("X-Keep", "original")
                    .header("x-test-add-headers", r#"{"xKeep":"override","newOne":"n"}"#),
            ),
        ] {
            mock.clear_requests();
            let answer = send_upgrade(&gw, &request).await?;
            ensure!(
                answer.status().starts_with("HTTP/1.1 101 "),
                "{label}: {}",
                answer.status()
            );
            let recorded = mock.last_request().context("forwarded")?;
            let lines = raw_header_lines(&recorded);
            let expected: &[&str] = if label == "plain" {
                &[
                    "host: 127.0.0.1:<port>",
                    "connection: Upgrade",
                    "upgrade: websocket",
                    "sec-websocket-version: 13",
                    "sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==",
                ]
            } else {
                &[
                    "host: 127.0.0.1:<port>",
                    "connection: Upgrade",
                    "upgrade: websocket",
                    "sec-websocket-version: 13",
                    "sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==",
                    "x-multi: one, two",
                    "cookie: a=1; b=2",
                    "user-agent: first",
                    "keep-alive: timeout=5",
                    "te: trailers",
                    "proxy-authorization: Basic eDp5",
                    "x-forwarded-for: 10.0.0.1",
                    "x-keep: override",
                    r#"x-test-add-headers: {"xKeep":"override","newOne":"n"}"#,
                    "new-one: n",
                ]
            };
            assert_eq!(lines, expected, "{label}");
            obs.record(label, &lines);
            obs.record(
                format!("{label}: method and version"),
                (&recorded.method, recorded.version),
            );
        }
        Ok(obs)
    })
    .await;
}

/// WS-09: the upstream connection is plain HTTP even for an `https` host, and every upgrade
/// opens a new connection, never one from the pool.
#[tokio::test]
async fn ws_09_plain_new_connection() {
    run(
        "ws_09_plain_new_connection",
        &["WS-09"],
        |target| async move {
            let mut obs = Observations::new();

            // An `https` host with a port: the upgrade still speaks plain HTTP to it.
            let plain = MockUpstream::start().await?;
            plain.set_document_json(fixtures::USERS_SERVICE);
            plain.set_default(Behaviour::WebSocketEcho);
            let mut config = GatewayConfig::new().service("svc", &plain);
            config.api_services[0].host = format!("https://{}", plain.addr());
            let gw = target.start(config).await?;
            let mut ws = open(&gw, &client::upgrade_request("/svc/socket")).await?;
            let echoed = round_trip(&mut ws, Message::text("over plain HTTP")).await?;
            assert_eq!(echoed, Message::text("over plain HTTP"));
            obs.record("https host", echoed.to_string());
            drop(ws);
            drop(gw);

            // Pooled HTTP connections are not used for upgrades.
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route("/svc/socket", Behaviour::WebSocketEcho);
            gw.send(&Request::get("/svc/v1/users")).await?;
            let mut sockets = Vec::new();
            for _ in 0..2 {
                sockets.push(open(&gw, &client::upgrade_request("/svc/socket")).await?);
            }
            gw.send(&Request::get("/svc/v1/users")).await?;
            let requests = mock.requests();
            let connection = |path: &str| -> Vec<u64> {
                requests
                    .iter()
                    .filter(|r| r.target == path)
                    .map(|r| r.connection)
                    .collect()
            };
            let http = connection("/v1/users/");
            let upgrades = connection("/svc/socket");
            ensure!(
                http.len() == 2 && upgrades.len() == 2,
                "requests: {requests:?}"
            );
            let facts = [
                (
                    "HTTP requests share a pooled connection",
                    http[0] == http[1],
                ),
                (
                    "upgrades use different connections",
                    upgrades[0] != upgrades[1],
                ),
                (
                    "upgrades do not use the pooled connection",
                    !upgrades.contains(&http[0]),
                ),
            ];
            for (fact, holds) in facts {
                assert!(holds, "{fact}: HTTP {http:?}, upgrades {upgrades:?}");
                obs.record(fact, holds);
            }
            // Close the tunnels gracefully before the gateway stops.
            for mut ws in sockets {
                let _ = ws.close(None).await;
            }
            Ok(obs)
        },
    )
    .await;
}

/// WS-10: the client gets `HTTP/1.1 101 Switching Protocols` and the upstream's 101 headers:
/// names lower-cased, repeated headers joined (or discarded) as Node does, `set-cookie` once
/// per value; the upstream's reason phrase is not used.
#[tokio::test]
async fn ws_10_switching_protocols_head() {
    run(
        "ws_10_switching_protocols_head",
        &["WS-10"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.set_default(Behaviour::WebSocketWithHead {
                reason: "Switching Now".into(),
                headers: vec![
                    ("X-Upstream".into(), "a".into()),
                    ("x-upstream".into(), "b".into()),
                    ("Set-Cookie".into(), "a=1".into()),
                    ("set-cookie".into(), "b=2".into()),
                    ("Content-Type".into(), "text/plain".into()),
                    ("content-type".into(), "text/html".into()),
                    ("X-Empty".into(), "".into()),
                ],
            });
            let Answer::Head(head, conn) =
                send_upgrade(&gw, &client::upgrade_request("/svc/socket")).await?
            else {
                bail!("no 101 head");
            };
            let head = String::from_utf8(head)?;
            assert_eq!(
                head,
                "HTTP/1.1 101 Switching Protocols\r\n\
             upgrade: websocket\r\n\
             connection: Upgrade\r\n\
             sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
             x-mock-upstream: ws\r\n\
             x-upstream: a, b\r\n\
             set-cookie: a=1\r\n\
             set-cookie: b=2\r\n\
             content-type: text/plain\r\n\
             x-empty: \r\n\
             \r\n"
            );
            let mut ws = conn.into_websocket().await;
            let echoed = round_trip(&mut ws, Message::text("after the head")).await?;
            assert_eq!(echoed, Message::text("after the head"));
            let _ = ws.close(None).await;
            let mut obs = Observations::new();
            obs.record("101 head", head);
            Ok(obs)
        },
    )
    .await;
}

/// WS-11: after the 101, bytes flow both ways unchanged.
#[tokio::test]
async fn ws_11_bytes_both_ways() {
    run("ws_11_bytes_both_ways", &["WS-11"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut ws = open(&gw, &client::upgrade_request("/svc/socket")).await?;
        let mut obs = Observations::new();
        let all_bytes: Vec<u8> = (0..=255u8).collect();
        let large: Vec<u8> = (0..1024 * 1024).map(|i: u32| (i % 251) as u8).collect();
        for (label, message) in [
            ("text", Message::text("hello, tunnel")),
            ("every byte value", Message::binary(all_bytes)),
            ("1 MiB", Message::binary(large)),
        ] {
            let echoed = round_trip(&mut ws, message.clone()).await?;
            assert!(echoed == message, "{label}: echo differs");
            obs.record(label, echoed.len());
        }
        for i in 0..100 {
            ws.feed(Message::text(format!("burst {i}"))).await?;
        }
        ws.flush().await?;
        let mut burst = Vec::new();
        for _ in 0..100 {
            burst.push(next_data(&mut ws).await?.into_text()?.to_string());
        }
        let expected: Vec<String> = (0..100).map(|i| format!("burst {i}")).collect();
        assert_eq!(burst, expected);
        obs.record("burst of 100 messages in order", true);
        let _ = ws.close(None).await;
        Ok(obs)
    })
    .await;
}

/// WS-12: bytes that arrive with the handshake are not lost: a frame the upstream sends in
/// the same write as its 101, and a frame the client sends in the same write as its upgrade
/// request.
#[tokio::test]
async fn ws_12_early_bytes() {
    run("ws_12_early_bytes", &["WS-12"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.route(
            "/svc/speak",
            Behaviour::WebSocketSpeakFirst("0{\"sid\":\"abc\"}".into()),
        );
        mock.route("/svc/echo", Behaviour::WebSocketEcho);
        let mut obs = Observations::new();

        let mut ws = open(&gw, &client::upgrade_request("/svc/speak")).await?;
        let first = next_data(&mut ws).await?;
        assert_eq!(first, Message::text("0{\"sid\":\"abc\"}"));
        obs.record("upstream speaks first", first.to_string());
        let _ = ws.close(None).await;

        let mut conn = RawConn::connect(gw.addr()).await?;
        let mut bytes = client::upgrade_request("/svc/echo").to_bytes(gw.addr());
        bytes.extend_from_slice(&masked_text_frame("early one"));
        bytes.extend_from_slice(&masked_text_frame("early two"));
        conn.write(&bytes).await?;
        let Answer::Head(head, conn) = answer(conn).await? else {
            bail!("no answer to the upgrade");
        };
        ensure!(
            status_line(&head).starts_with("HTTP/1.1 101 "),
            "{}",
            status_line(&head)
        );
        let mut ws = conn.into_websocket().await;
        let echoed = [next_data(&mut ws).await?, next_data(&mut ws).await?];
        assert_eq!(
            echoed,
            [Message::text("early one"), Message::text("early two")]
        );
        obs.record(
            "client frames sent with the handshake",
            echoed.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
        );
        let _ = ws.close(None).await;
        Ok(obs)
    })
    .await;
}

/// How long WS-13 keeps a tunnel idle. It must outlast every idle timer that could
/// plausibly apply: Node's `keepAliveTimeout` (5 s), which the reference's socket carries
/// after it served a keep-alive request and which the Rust gateway advertises
/// (`Keep-Alive: timeout=5`), and undici's pooled-socket keep-alive (4 s). Node's
/// `headersTimeout` (60 s) and `requestTimeout` (300 s) stop applying once the head is
/// parsed; hyper and tokio apply no idle timeout unless configured.
const IDLE: Duration = Duration::from_secs(6);

/// WS-13: a tunnel has no idle timeout, even on a client connection that served a
/// keep-alive request before upgrading.
#[tokio::test]
async fn ws_13_no_idle_timeout() {
    run("ws_13_no_idle_timeout", &["WS-13"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.route("/svc/socket", Behaviour::WebSocketEcho);
        let mut conn = RawConn::connect(gw.addr()).await?;
        conn.send(&Request::get("/svc/v1/users")).await?;
        let first = conn.read_response(false, WAIT).await?;
        ensure!(first.status == 200, "keep-alive request: {}", first.status);
        conn.send(&client::upgrade_request("/svc/socket")).await?;
        let head = conn.read_head_raw(WAIT).await?.context("no 101")?;
        ensure!(
            status_line(&head).starts_with("HTTP/1.1 101 "),
            "{}",
            status_line(&head)
        );
        let mut ws = conn.into_websocket().await;
        tokio::time::sleep(IDLE).await;
        let echoed = round_trip(&mut ws, Message::text("still here")).await?;
        assert_eq!(echoed, Message::text("still here"));
        let _ = ws.close(None).await;
        let mut obs = Observations::new();
        obs.record(
            format!("echo after {} s idle", IDLE.as_secs()),
            echoed.to_string(),
        );
        Ok(obs)
    })
    .await;
}

/// WS-14: when either side closes, the other side is closed.
#[tokio::test]
async fn ws_14_close_propagation() {
    run("ws_14_close_propagation", &["WS-14"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        mock.set_default(Behaviour::WebSocketEcho);
        let mut obs = Observations::new();

        // The client closes its TCP connection: the upstream connection is closed.
        let mut ws = open(&gw, &client::upgrade_request("/svc/socket")).await?;
        round_trip(&mut ws, Message::text("before closing")).await?;
        let before = mock.websockets_ended();
        drop(ws);
        mock.wait_for_websockets_ended(before + 1, WAIT).await?;
        obs.record("client closes: upstream connection closed", true);

        // The upstream closes (after a Close frame): the client connection is closed.
        let Answer::Head(head, mut conn) =
            send_upgrade(&gw, &client::upgrade_request("/svc/socket")).await?
        else {
            bail!("no 101");
        };
        ensure!(
            status_line(&head).starts_with("HTTP/1.1 101 "),
            "{}",
            status_line(&head)
        );
        conn.write(&masked_close_frame()).await?;
        let rest = conn.read_to_close(WAIT).await?;
        obs.record("upstream closes: client connection closed", true);
        obs.record("bytes before the close", rest.len());
        Ok(obs)
    })
    .await;
}

/// WS-16: when the upstream cannot be reached, the client connection is closed with no
/// response.
#[tokio::test]
async fn ws_16_upstream_unreachable() {
    run(
        "ws_16_upstream_unreachable",
        &["WS-16"],
        |target| async move {
            let docs = MockUpstream::start().await?;
            docs.set_document_json(fixtures::USERS_SERVICE);
            // A port nothing listens on.
            let closed = TcpListener::bind("127.0.0.1:0").await?.local_addr()?;
            let mut config = GatewayConfig::new().service("svc", &docs);
            config.api_services[0].host = format!("http://{closed}");
            let gw = target.start(config).await?;
            let answer = send_upgrade(&gw, &client::upgrade_request("/svc/socket")).await?;
            let seen = answer.status();
            assert_eq!(seen, "closed, no response");
            let mut obs = Observations::new();
            obs.record("upstream unreachable", seen);
            Ok(obs)
        },
    )
    .await;
}
