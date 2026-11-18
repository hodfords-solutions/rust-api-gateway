//! Security tests: resource limits and slow clients (`docs/security/attack-tests.md`).
//!
//! Every test is bounded to a few seconds: the timeout tests (SEC-02 and SEC-03, fixed as
//! REQ-28 and REQ-29) configure timeouts far shorter than Node's defaults.

mod gateway_common;
mod websocket_common;

use std::time::{Duration, Instant};

use api_gateway::gateway::Gateway;
use api_gateway::server::ServerTimeouts;
use api_gateway::upstream::UpstreamService;
use gateway_common::{Served, Upstream, options, parse_responses, raw_send, serve};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// Node's answer to a request that took too long.
const REQUEST_TIMEOUT: &[u8] = b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n";

/// Short timeouts for the tests.
fn short() -> ServerTimeouts {
    ServerTimeouts {
        headers_timeout: Duration::from_millis(400),
        request_timeout: Duration::from_millis(800),
        keep_alive_timeout: Duration::from_secs(1),
        keep_alive_timeout_buffer: Duration::from_millis(200),
    }
}

async fn setup() -> (Upstream, Served) {
    let upstream = Upstream::start().await;
    let mut options = options(vec![upstream.service("svc")]);
    options.server_timeouts = short();
    let served = serve(Gateway::builder(options), true).await;
    (upstream, served)
}

/// Reads until the gateway closes the connection (at most 3 s); returns what arrived and when
/// the connection closed, measured from `started`.
async fn read_until_closed(
    read: &mut (impl AsyncRead + Unpin),
    started: Instant,
) -> (Vec<u8>, Duration) {
    let mut received = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), read.read_to_end(&mut received)).await;
    (received, started.elapsed())
}

/// Writes `byte` every 100 ms until the connection fails.
fn trickle(
    mut write: tokio::net::tcp::OwnedWriteHalf,
    byte: &'static [u8],
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if write.write_all(byte).await.is_err() {
                return;
            }
        }
    })
}

async fn still_serves(served: &Served) {
    let raw = raw_send(
        served.addr,
        b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(
        parse_responses(&raw).first().map(|r| r.status),
        Some(200),
        "the gateway still serves"
    );
}

/// A very large request body is streamed to the upstream, not buffered: the upstream
/// receives the request while almost all of a declared 100 MiB body has yet to be sent, so
/// the gateway's memory does not grow with the body.
#[tokio::test]
async fn large_request_bodies_are_streamed_not_buffered() {
    let (upstream, served) = setup().await;
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(b"POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nContent-Length: 104857600\r\n\r\n")
        .await
        .expect("head");
    stream.write_all(&[b'a'; 64 * 1024]).await.expect("body");
    let started = Instant::now();
    while upstream.seen().is_empty() && started.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(upstream.seen().len(), 1, "forwarded before the body ended");
    drop(stream);
    still_serves(&served).await;
}

/// Chunk sizes that overflow or are malformed fail the request without forwarding it, and do
/// not take the gateway down.
#[tokio::test]
async fn overflowing_chunk_sizes_fail_the_request_only() {
    let (upstream, served) = setup().await;
    for size in ["ffffffffffffffffff", "+5", "-1", "5 5"] {
        let request = format!(
            "POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nTransfer-Encoding: chunked\r\n\r\n{size}\r\nhello\r\n0\r\n\r\n"
        );
        let raw = raw_send(served.addr, request.as_bytes(), Duration::from_secs(2)).await;
        let status = parse_responses(&raw).first().map(|r| r.status);
        assert!(status.is_none_or(|s| s >= 400), "{size}: {status:?}");
        assert!(upstream.seen().is_empty(), "{size}: forwarded");
    }
    still_serves(&served).await;
}

/// SEC-02, fixed (REQ-28): a client that sends its request head a byte at a time
/// (slowloris) is answered `408 Request Timeout` and the connection is closed once
/// `headersTimeout` has passed since the head's first byte.
#[tokio::test]
async fn sec_02_slow_request_heads_are_answered_408_and_closed() {
    let (upstream, served) = setup().await;
    let stream = TcpStream::connect(served.addr).await.expect("connect");
    let started = Instant::now();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n")
        .await
        .expect("partial head");
    let trickling = trickle(write, b"X");
    let (received, closed) = read_until_closed(&mut read, started).await;
    trickling.abort();
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(REQUEST_TIMEOUT)
    );
    assert!(
        closed >= Duration::from_millis(400) && closed < Duration::from_millis(1500),
        "closed after {closed:?}"
    );
    assert!(upstream.seen().is_empty());
}

/// REQ-28: a connection that never sends anything counts as a request whose head has not
/// arrived: it gets the 408 after `headersTimeout`, as in Node.
#[tokio::test]
async fn a_connection_that_sends_nothing_is_answered_408_and_closed() {
    let (_upstream, served) = setup().await;
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    let (received, closed) = read_until_closed(&mut stream, Instant::now()).await;
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(REQUEST_TIMEOUT)
    );
    assert!(
        closed < Duration::from_millis(1500),
        "closed after {closed:?}"
    );
}

/// SEC-02, fixed (REQ-28): a request body that arrives a byte at a time is answered 408 and
/// the connection closed once `requestTimeout` has passed since the request started.
#[tokio::test]
async fn sec_02_slow_request_bodies_are_answered_408_and_closed() {
    let (_upstream, served) = setup().await;
    let stream = TcpStream::connect(served.addr).await.expect("connect");
    let started = Instant::now();
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(b"POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nContent-Length: 100000\r\n\r\n")
        .await
        .expect("head");
    let trickling = trickle(write, b"a");
    let (received, closed) = read_until_closed(&mut read, started).await;
    trickling.abort();
    assert_eq!(
        String::from_utf8_lossy(&received),
        String::from_utf8_lossy(REQUEST_TIMEOUT)
    );
    assert!(
        closed >= Duration::from_millis(800) && closed < Duration::from_millis(2000),
        "closed after {closed:?}"
    );
}

/// REQ-28: the timeouts bound receiving the request, not answering it: a complete request
/// whose upstream takes longer than `requestTimeout` to answer gets its answer.
#[tokio::test]
async fn a_slow_upstream_answer_is_not_cut_short() {
    let (_upstream, served) = setup().await;
    let raw = raw_send(
        served.addr,
        b"GET /svc/v1/users?delay=1200 HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(parse_responses(&raw).first().map(|r| r.status), Some(200));
}

/// SEC-03, fixed (REQ-29): after a response the connection may stay idle for
/// `keepAliveTimeout` (announced as `Keep-Alive: timeout=<seconds>`) plus
/// `keepAliveTimeoutBuffer`, and is then closed without a response; a client reusing it
/// before then is served, and that restarts the count.
#[tokio::test]
async fn sec_03_idle_keep_alive_connections_are_closed() {
    let (upstream, served) = setup().await;
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    let mut buffer = vec![0u8; 8192];
    for _ in 0..2 {
        stream
            .write_all(b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n\r\n")
            .await
            .expect("request");
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer))
            .await
            .expect("response in time")
            .expect("read");
        let response = parse_responses(&buffer[..n]);
        assert_eq!(response.first().map(|r| r.status), Some(200));
        assert_eq!(
            response.first().and_then(|r| r.header("keep-alive")),
            Some("timeout=1")
        );
        // Within the timeout: the connection stays open for the next request.
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    assert_eq!(upstream.seen().len(), 2, "both requests on one connection");
    let (received, closed) = read_until_closed(&mut stream, Instant::now()).await;
    assert!(
        received.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&received)
    );
    // 1.2 s after the last response, 0.8 s of which already passed.
    assert!(
        closed < Duration::from_millis(1200),
        "closed after {closed:?} more"
    );
    assert!(
        closed >= Duration::from_millis(200),
        "closed after {closed:?} more"
    );
}

/// A WebSocket tunnel is handed over with its upgrade head, so the client connection
/// timeouts never close it (WS-13).
#[tokio::test]
async fn websocket_tunnels_never_time_out() {
    let echo = websocket_common::EchoUpstream::start().await;
    let mut options = options(vec![UpstreamService::new(
        "svc",
        format!("http://{}/openapi.json", echo.addr),
        format!("http://{}", echo.addr),
    )]);
    options.server_timeouts = short();
    let served = serve(Gateway::builder(options), true).await;
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(&websocket_common::upgrade_request("/svc/x", ""))
        .await
        .expect("upgrade");
    assert!(
        websocket_common::read_head(&mut stream)
            .await
            .starts_with(b"HTTP/1.1 101")
    );
    tokio::time::sleep(Duration::from_millis(1800)).await;
    stream.write_all(b"still here").await.expect("write");
    let mut back = [0u8; 10];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut back))
        .await
        .expect("echo in time")
        .expect("read");
    assert_eq!(&back, b"still here");
}
