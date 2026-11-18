//! Unit tests, ported from the reference's `proxy/proxy-socket.spec.ts` and
//! `restful/services/ws-request.service.spec.ts`.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{DuplexStream, duplex};

use super::*;
use crate::handler::MiddlewareError;

fn single(value: &str) -> NodeHeaderValue {
    NodeHeaderValue::Single(value.as_bytes().to_vec())
}

fn entries(pairs: &[(&str, &str)]) -> Vec<(String, NodeHeaderValue)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), single(v)))
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// proxy-socket.spec.ts: `ProxySocket.checkMethodAndHeader` (WS-06).

#[test]
fn accepts_a_get_request_with_a_websocket_upgrade_header() {
    assert!(is_websocket_upgrade(&Method::GET, Some(b"websocket")));
}

#[test]
fn accepts_a_case_insensitive_upgrade_value() {
    assert!(is_websocket_upgrade(&Method::GET, Some(b"WebSocket")));
}

#[test]
fn rejects_non_get_methods() {
    assert!(!is_websocket_upgrade(&Method::POST, Some(b"websocket")));
}

#[test]
fn rejects_a_missing_or_non_websocket_upgrade_header() {
    assert!(!is_websocket_upgrade(&Method::GET, None));
    assert!(!is_websocket_upgrade(&Method::GET, Some(b"h2c")));
    // Node joins repeated lines before the check.
    assert!(!is_websocket_upgrade(&Method::GET, Some(b"websocket, h2c")));
}

// "destroys the socket when handling an invalid upgrade request": `prepare` answers
// `Refusal::Drop`, which closes the connection without writing; covered end to end by
// `tests/gateway_websocket.rs` and the Conformance Test `websocket::ws_06_not_a_websocket`.

// proxy-socket.spec.ts: `ProxySocket.createHttpHeader` (WS-10, WS-15).

#[test]
fn serializes_scalar_headers_after_the_status_line() {
    let head = http_head(
        "HTTP/1.1 101 Switching Protocols",
        &entries(&[("upgrade", "websocket"), ("connection", "Upgrade")]),
    );
    assert_eq!(
        text(&head),
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\r\n"
    );
}

#[test]
fn repeats_array_header_values_on_separate_lines() {
    let head = http_head(
        "HTTP/1.1 200 OK",
        &[(
            "set-cookie".into(),
            NodeHeaderValue::Multiple(vec![b"a=1".to_vec(), b"b=2".to_vec()]),
        )],
    );
    assert_eq!(
        text(&head),
        "HTTP/1.1 200 OK\r\nset-cookie: a=1\r\nset-cookie: b=2\r\n\r\n"
    );
}

#[test]
fn terminates_an_empty_header_set_correctly() {
    assert_eq!(
        text(&http_head("HTTP/1.1 200 OK", &[])),
        "HTTP/1.1 200 OK\r\n\r\n"
    );
}

#[test]
fn writes_latin1_header_bytes_as_utf8_like_node() {
    let head = http_head(
        "HTTP/1.1 101 Switching Protocols",
        &[("x-name".into(), NodeHeaderValue::Single(vec![0xe9]))],
    );
    assert_eq!(
        text(&head),
        "HTTP/1.1 101 Switching Protocols\r\nx-name: \u{e9}\r\n\r\n"
    );
}

#[test]
fn the_101_head_joins_repeated_upstream_headers_like_node() {
    let raw = b"HTTP/1.1 101 Whatever\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nX-A: 1\r\nx-a: 2\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nServer: one\r\nServer: two\r\n\r\n";
    let response = parse_response_head(raw).expect("valid head");
    assert_eq!(response.status, 101);
    assert_eq!(
        text(&switching_protocols(&response, b"")),
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nx-a: 1, 2\r\nset-cookie: a=1\r\nset-cookie: b=2\r\nserver: one\r\n\r\n"
    );
}

#[test]
fn a_refused_upgrade_gets_the_upstream_status_line_only() {
    let raw = b"HTTP/1.1 401 Nope Nope\r\nX-Hidden: 1\r\n\r\n";
    let response = parse_response_head(raw).expect("valid head");
    assert_eq!(response.reason, b"Nope Nope");
    assert_eq!(
        refusal_line(Version::HTTP_11, response.status, &response.reason),
        "HTTP/1.1 401 Nope Nope"
    );
    assert_eq!(refusal_line(Version::HTTP_10, 503, b""), "HTTP/1.0 503 ");
    let response = parse_response_head(b"HTTP/1.1 418\r\n\r\n").expect("valid head");
    assert_eq!(response.reason, b"");
    assert!(
        parse_response_head(
            b"HTTP/1.1 200 OK\r\ncontent-length: 1\r\ntransfer-encoding: chunked\r\n\r\n"
        )
        .is_none()
    );
}

// proxy-socket.spec.ts: "ProxySocket upgrade head" (WS-12).

/// Runs [`splice`] between two in-memory connections; returns the far ends (what the
/// client and the upstream see).
fn spliced(first: &'static [u8], early: &'static [u8]) -> (DuplexStream, DuplexStream) {
    let (client, client_far) = duplex(64 * 1024);
    let (upstream, upstream_far) = duplex(64 * 1024);
    tokio::spawn(async move {
        let _ = splice(client, upstream, first, early).await;
    });
    (client_far, upstream_far)
}

async fn read_exactly(stream: &mut DuplexStream, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .expect("bytes arrive")
        .expect("stream open");
    buf
}

#[tokio::test]
async fn replays_the_upstream_head_so_a_server_spoken_first_frame_survives() {
    let response =
        parse_response_head(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n")
            .expect("head");
    let open_packet = b"\x81\x1f0{\"sid\":\"abc\",\"pingInterval\":25000}";
    let first = switching_protocols(&response, open_packet);
    assert!(first.ends_with(open_packet));
    let first: &'static [u8] = Box::leak(first.into_boxed_slice());
    let (mut client, _upstream) = spliced(first, b"");
    assert_eq!(read_exactly(&mut client, first.len()).await, first);
}

#[tokio::test]
async fn replays_the_upstream_head_before_wiring_the_pipes() {
    let (mut client, mut upstream) = spliced(b"HEAD|frame", b"");
    upstream.write_all(b"piped").await.expect("write");
    assert_eq!(read_exactly(&mut client, 15).await, b"HEAD|framepiped");
}

#[tokio::test]
async fn replays_the_client_head_onto_the_upstream() {
    let (mut client, mut upstream) = spliced(b"", b"early-client-frame");
    client.write_all(b"|later").await.expect("write");
    assert_eq!(
        read_exactly(&mut upstream, 24).await,
        b"early-client-frame|later"
    );
}

#[tokio::test]
async fn does_not_replay_an_absent_or_empty_head() {
    let (mut client, mut upstream) = spliced(b"", b"");
    client.write_all(b"c").await.expect("write");
    upstream.write_all(b"u").await.expect("write");
    assert_eq!(read_exactly(&mut upstream, 1).await, b"c");
    assert_eq!(read_exactly(&mut client, 1).await, b"u");
}

// "retains the client head handed to handleWebsocket": the server keeps the bytes after the
// upgrade head (`server::inbound::tests::upgrade_heads_are_held_with_what_follows`).

// proxy-socket.spec.ts: "ProxySocket upstream idle timer" (WS-09, WS-13).

#[tokio::test]
async fn opts_out_of_the_pool_for_the_upgrade_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("address").port();
    let outgoing = Outgoing {
        upstream: UpstreamAddress::from_host(&format!("http://127.0.0.1:{port}")).expect("host"),
        request: b"GET / HTTP/1.1\r\n\r\n".to_vec(),
    };
    let _first = connect(&outgoing).await.expect("first connection");
    let _second = connect(&outgoing).await.expect("second connection");
    for _ in 0..2 {
        let (mut accepted, _) = listener.accept().await.expect("a new connection each time");
        let mut buf = [0u8; 18];
        accepted.read_exact(&mut buf).await.expect("request");
        assert_eq!(&buf, b"GET / HTTP/1.1\r\n\r\n");
    }
}

#[test]
fn still_forwards_path_method_and_merged_headers() {
    let upstream = UpstreamAddress::from_host("http://localhost:18080").expect("host");
    assert_eq!(
        (upstream.connect_host.as_str(), upstream.port),
        ("localhost", 18080)
    );
    let head = upstream_request_head(
        &Method::GET,
        "/socket.io/?EIO=4",
        entries(&[("upgrade", "websocket"), ("cookie", "a=1")]),
        &[("auth-user-id".into(), "u-1".into())],
        &upstream,
    )
    .expect("valid");
    assert_eq!(
        text(&head),
        "GET /socket.io/?EIO=4 HTTP/1.1\r\nupgrade: websocket\r\ncookie: a=1\r\nauth-user-id: u-1\r\nHost: localhost:18080\r\nConnection: close\r\n\r\n"
    );
}

#[tokio::test(start_paused = true)]
async fn a_quiet_tunnel_is_never_reclaimed() {
    let (mut client, mut upstream) = spliced(b"", b"");
    tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
    client.write_all(b"ping").await.expect("write");
    assert_eq!(read_exactly(&mut upstream, 4).await, b"ping");
    upstream.write_all(b"pong").await.expect("write");
    assert_eq!(read_exactly(&mut client, 4).await, b"pong");
}

// WS-07 to WS-09 beyond the spec.

#[test]
fn middleware_headers_replace_in_place_and_nothing_is_added() {
    let upstream = UpstreamAddress::from_host("https://gw.example").expect("host");
    let head = upstream_request_head(
        &Method::GET,
        "/svc/socket?a=1&b",
        vec![
            ("host".into(), single("gateway:3000")),
            ("connection".into(), single("Upgrade")),
            ("x-keep".into(), single("original")),
            ("x-forwarded-for".into(), single("10.0.0.1")),
            (
                "set-cookie".into(),
                NodeHeaderValue::Multiple(vec![b"a=1".to_vec(), b"b=2".to_vec()]),
            ),
        ],
        &[
            ("x-keep".into(), "override".into()),
            ("new-one".into(), "n".into()),
        ],
        &upstream,
    )
    .expect("valid");
    assert_eq!(
        text(&head),
        "GET /svc/socket?a=1&b HTTP/1.1\r\nhost: gateway:3000\r\nconnection: Upgrade\r\nx-keep: override\r\nx-forwarded-for: 10.0.0.1\r\nset-cookie: a=1\r\nset-cookie: b=2\r\nnew-one: n\r\n\r\n"
    );
}

#[test]
fn invalid_middleware_headers_fail_like_node_throws() {
    let upstream = UpstreamAddress::from_host("http://a").expect("host");
    for extra in [
        ("bad name", "x"),
        ("x-a", "line\nbreak"),
        ("x-a", "\u{100}"),
    ] {
        let result = upstream_request_head(
            &Method::GET,
            "/",
            Vec::new(),
            &[(extra.0.into(), extra.1.into())],
            &upstream,
        );
        assert!(
            matches!(result, Err(GatewayError::UpstreamUnavailable)),
            "{extra:?}"
        );
    }
    // Latin-1 characters are written as single bytes.
    let head = upstream_request_head(
        &Method::GET,
        "/",
        Vec::new(),
        &[("x-a".into(), "\u{e9}".into())],
        &upstream,
    )
    .expect("latin1 is fine");
    assert!(head.windows(6).any(|w| w == b"x-a: \xe9"));
}

#[test]
fn upstream_addresses_are_plain_http() {
    let address =
        |host: &str| UpstreamAddress::from_host(host).map(|a| (a.connect_host, a.hostname, a.port));
    assert_eq!(
        address("http://svc:4000"),
        Some(("svc".into(), "svc".into(), 4000))
    );
    assert_eq!(
        address("https://svc:8443"),
        Some(("svc".into(), "svc".into(), 8443))
    );
    // `new URL` drops a scheme's default port; Node's `http.request` then uses 80.
    assert_eq!(
        address("https://svc"),
        Some(("svc".into(), "svc".into(), 80))
    );
    assert_eq!(
        address("https://svc:443"),
        Some(("svc".into(), "svc".into(), 80))
    );
    assert_eq!(
        address("http://[::1]:5000"),
        Some(("::1".into(), "[::1]".into(), 5000))
    );
    assert_eq!(address("not a url"), None);
}

#[test]
fn host_header_is_added_when_the_client_sent_none() {
    let upstream = UpstreamAddress::from_host("http://[::1]").expect("host");
    let head = upstream_request_head(
        &Method::GET,
        "/",
        entries(&[("connection", "Upgrade")]),
        &[],
        &upstream,
    )
    .expect("valid");
    assert_eq!(
        text(&head),
        "GET / HTTP/1.1\r\nconnection: Upgrade\r\nHost: [::1]\r\n\r\n"
    );
}

// WS-15: the body of a refused upgrade.

async fn body_of(raw: &'static [u8], head: &'static [u8]) -> Vec<u8> {
    let response = parse_response_head(head).expect("head");
    let mut body = UpstreamBody::new(&response, BytesMut::new());
    let mut upstream = raw;
    let mut out = Vec::new();
    while let Some(data) = body.next(&mut upstream).await.expect("body") {
        assert!(!data.is_empty(), "no empty pieces");
        out.extend_from_slice(&data);
    }
    out
}

#[tokio::test]
async fn refused_bodies_are_decoded_from_their_framing() {
    assert_eq!(
        body_of(
            b"hello, extra",
            b"HTTP/1.1 401 X\r\ncontent-length: 5\r\n\r\n"
        )
        .await,
        b"hello"
    );
    assert_eq!(
        body_of(
            b"3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nx-trailer: 1\r\n\r\n",
            b"HTTP/1.1 401 X\r\ntransfer-encoding: chunked\r\n\r\n"
        )
        .await,
        b"abcde"
    );
    assert_eq!(
        body_of(b"until close", b"HTTP/1.1 401 X\r\n\r\n").await,
        b"until close"
    );
    assert_eq!(body_of(b"ignored", b"HTTP/1.1 304 X\r\n\r\n").await, b"");
}

// ws-request.service.spec.ts: `WsRequestService.handle` (WS-04).

struct Recording {
    allow: bool,
    add: Option<(&'static str, &'static str)>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
}

#[async_trait]
impl WsProxyMiddleware for Recording {
    async fn handle(
        &self,
        _request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        self.calls.lock().expect("lock").push(self.name);
        if let Some((name, value)) = self.add {
            proxy_request.add_header(name, value);
        }
        Ok(self.allow)
    }
}

struct Failing;

#[async_trait]
impl WsProxyMiddleware for Failing {
    async fn handle(
        &self,
        _request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        Err(MiddlewareError::json(
            http::StatusCode::UNAUTHORIZED,
            serde_json::json!({ "message": "no" }),
        ))
    }
}

fn request() -> GatewayRequest {
    GatewayRequest::new(
        Method::GET,
        Uri::from_static("/svc/socket"),
        "/svc/socket",
        HeaderMap::new(),
        None,
    )
}

fn recording(
    name: &'static str,
    allow: bool,
    add: Option<(&'static str, &'static str)>,
    calls: &Arc<Mutex<Vec<&'static str>>>,
) -> Arc<dyn WsProxyMiddleware> {
    Arc::new(Recording {
        allow,
        add,
        calls: Arc::clone(calls),
        name,
    })
}

#[tokio::test]
async fn returns_true_when_there_are_no_handlers() {
    let mut proxy_request = ProxyRequest::new();
    let result = run_middleware(&Vec::new(), &[], &mut request(), &mut proxy_request).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn returns_false_when_a_handler_rejects_the_upgrade_and_stops_the_chain() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = vec![
        recording("first", true, None, &calls),
        recording("second", false, None, &calls),
        recording("third", true, None, &calls),
    ];
    let result = run_middleware(&chain, &[], &mut request(), &mut ProxyRequest::new()).await;
    assert!(matches!(result, Err(GatewayError::Forbidden)));
    assert_eq!(*calls.lock().expect("lock"), ["first", "second"]);

    let failing: Vec<Arc<dyn WsProxyMiddleware>> = vec![Arc::new(Failing)];
    let result = run_middleware(&failing, &[], &mut request(), &mut ProxyRequest::new()).await;
    assert!(matches!(result, Err(GatewayError::Middleware { status, .. }) if status == 401));
}

#[tokio::test]
async fn passes_the_proxy_request_through_each_handler() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let chain = vec![
        recording("first", true, Some(("authUserId", "123")), &calls),
        recording("second", true, Some(("xTrace", "t")), &calls),
    ];
    let mut proxy_request = ProxyRequest::new();
    run_middleware(&chain, &[], &mut request(), &mut proxy_request)
        .await
        .expect("allowed");
    assert_eq!(proxy_request.header("authUserId"), Some("123"));
    assert_eq!(
        proxy_request.kebab_headers(),
        [
            ("auth-user-id".to_string(), "123".to_string()),
            ("x-trace".to_string(), "t".to_string())
        ]
    );
}

#[tokio::test]
async fn drops_excluded_headers_that_are_already_present_on_the_proxy_request() {
    let mut proxy_request = ProxyRequest::new();
    proxy_request.add_header("auth-user-id", "spoofed");
    run_middleware(
        &Vec::new(),
        &["auth-user-id".to_string()],
        &mut request(),
        &mut proxy_request,
    )
    .await
    .expect("allowed");
    assert_eq!(proxy_request.header("auth-user-id"), None);
}

#[test]
fn strips_caller_supplied_excluded_headers_from_the_upgrade_request_itself() {
    let lines = vec![
        (
            HeaderName::from_static("auth-user-id"),
            HeaderValue::from_static("spoofed"),
        ),
        (
            HeaderName::from_static("upgrade"),
            HeaderValue::from_static("websocket"),
        ),
    ];
    let kept = strip_excluded(&lines, &["Auth-User-Id".to_string()]);
    assert_eq!(
        kept,
        [(
            HeaderName::from_static("upgrade"),
            HeaderValue::from_static("websocket")
        )]
    );
}
