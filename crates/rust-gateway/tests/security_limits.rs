//! Security tests: resource limits and slow clients (`docs/security/attack-tests.md`).
//!
//! Every test is bounded to a few seconds. The timeout tests cannot prove that a timeout is
//! missing, only that none fires within their bound: [`BOUND`] is the bound the safe
//! behaviour is checked against, to be set to whatever timeout the Deviation that fixes
//! SEC-02 and SEC-03 chooses.

mod gateway_common;

use std::time::{Duration, Instant};

use api_gateway::gateway::Gateway;
use gateway_common::{Served, Upstream, closed_within, options, parse_responses, raw_send, serve};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// How long a slow or idle client may hold a connection in the safe behaviour of SEC-02 and
/// SEC-03.
const BOUND: Duration = Duration::from_secs(5);

async fn setup() -> (Upstream, Served) {
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    (upstream, served)
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

/// SEC-02 (slowloris): a client that sends its request head one byte at a time holds the
/// connection for as long as it keeps sending; the server has no header read timeout. The
/// Reference Gateway runs on Node, whose `headersTimeout` (60 s) bounds it.
///
/// Safe behaviour: the connection is closed (or answered with 408) within [`BOUND`].
#[tokio::test]
#[ignore = "security finding SEC-02: no timeout for a request head that arrives slowly"]
async fn sec_02_slow_request_heads_time_out() {
    let (upstream, served) = setup().await;
    let stream = TcpStream::connect(served.addr).await.expect("connect");
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n")
        .await
        .expect("partial head");
    let trickle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if write.write_all(b"X").await.is_err() {
                return;
            }
        }
    });
    let closed = closed_within(&mut read, BOUND + Duration::from_secs(1)).await;
    trickle.abort();
    assert!(closed, "a slow head held the connection beyond {BOUND:?}");
    assert!(upstream.seen().is_empty());
}

/// SEC-02 (slow body): a request body that arrives a byte at a time holds the client
/// connection and an upstream connection for as long as the client keeps sending; there is
/// no request timeout. Node bounds a whole request with `requestTimeout` (300 s).
///
/// Safe behaviour: the connection is closed (or answered with 408) within [`BOUND`].
#[tokio::test]
#[ignore = "security finding SEC-02: no timeout for a request body that arrives slowly"]
async fn sec_02_slow_request_bodies_time_out() {
    let (_upstream, served) = setup().await;
    let stream = TcpStream::connect(served.addr).await.expect("connect");
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(b"POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nContent-Length: 100000\r\n\r\n")
        .await
        .expect("head");
    let trickle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if write.write_all(b"a").await.is_err() {
                return;
            }
        }
    });
    let closed = closed_within(&mut read, BOUND + Duration::from_secs(1)).await;
    trickle.abort();
    assert!(closed, "a slow body held the connection beyond {BOUND:?}");
}

/// SEC-03: after a response, the gateway announces `Keep-Alive: timeout=5` but never closes
/// the idle connection, so idle clients accumulate connections (and file descriptors) without
/// limit. Node, under the Reference Gateway, closes it after 5 s (`keepAliveTimeout`).
///
/// Safe behaviour: the connection is closed within the announced timeout (plus slack).
#[tokio::test]
#[ignore = "security finding SEC-03: idle keep-alive connections are never closed"]
async fn sec_03_idle_keep_alive_connections_are_closed() {
    let (_upstream, served) = setup().await;
    let mut stream = TcpStream::connect(served.addr).await.expect("connect");
    stream
        .write_all(b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n\r\n")
        .await
        .expect("request");
    let mut buffer = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buffer))
        .await
        .expect("response in time")
        .expect("read");
    let response = parse_responses(&buffer[..n]);
    let announced = response
        .first()
        .and_then(|r| r.header("keep-alive"))
        .and_then(|v| v.strip_prefix("timeout="))
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .expect("a keep-alive timeout is announced");
    assert!(
        closed_within(&mut stream, announced + Duration::from_millis(1500)).await,
        "an idle connection stayed open beyond the announced {announced:?}"
    );
}
