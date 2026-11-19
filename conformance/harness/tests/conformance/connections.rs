//! Client connections: REQ-28 (request timeouts) and REQ-29 (keep-alive timeout).
//!
//! Node's defaults (60 s, 300 s, 5 s) are too long for a scenario, so the gateway is started
//! with short `http.Server` timeouts (the harness `server` field). The reference checks the
//! first two every `connectionsCheckingInterval`, set to 100 ms here; the Rust target checks
//! them exactly. Timings are observed as "inside the expected window" only.

use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use conformance::client::Request;
use conformance::runner::{Observations, run};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::common::users_gateway;

const HEADERS_TIMEOUT: Duration = Duration::from_millis(1000);
const REQUEST_TIMEOUT: Duration = Duration::from_millis(2000);
const KEEP_ALIVE: Duration = Duration::from_millis(1000);
const KEEP_ALIVE_BUFFER: Duration = Duration::from_millis(500);

fn server() -> serde_json::Value {
    json!({
        "headersTimeout": HEADERS_TIMEOUT.as_millis() as u64,
        "requestTimeout": REQUEST_TIMEOUT.as_millis() as u64,
        "keepAliveTimeout": KEEP_ALIVE.as_millis() as u64,
        "keepAliveTimeoutBuffer": KEEP_ALIVE_BUFFER.as_millis() as u64,
        "connectionsCheckingInterval": 100,
    })
}

/// Sends `first`, then `trickle` every 100 ms, and returns what arrived until the gateway
/// closed the connection (at most 5 s) and when it closed.
async fn slow_client(
    addr: std::net::SocketAddr,
    first: &'static [u8],
    trickle: &'static [u8],
) -> Result<(String, Duration)> {
    let stream = TcpStream::connect(addr).await?;
    let started = Instant::now();
    let (mut read, mut write) = stream.into_split();
    write.write_all(first).await?;
    let writer = tokio::spawn(async move {
        if trickle.is_empty() {
            // Keep the write half open without sending anything.
            std::future::pending::<()>().await;
        }
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if write.write_all(trickle).await.is_err() {
                return;
            }
        }
    });
    let mut received = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), read.read_to_end(&mut received)).await;
    let closed = started.elapsed();
    writer.abort();
    Ok((String::from_utf8_lossy(&received).into_owned(), closed))
}

/// REQ-28: a request head that has not arrived after `headersTimeout` (counted from the
/// connection's start for its first request, even when nothing was sent), and a request
/// body that has not arrived after `requestTimeout`, are answered
/// `HTTP/1.1 408 Request Timeout` with `Connection: close`, and the connection is closed.
#[tokio::test]
async fn req_28_request_timeouts() {
    run(
        "req_28_request_timeouts",
        &["REQ-28"],
        |target| async move {
            let (_mock, gw) = users_gateway(&target, |c| c.server(server())).await?;
            let addr = gw.addr();
            let (slow_head, silent, slow_body) = tokio::join!(
                slow_client(addr, b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\n", b"X"),
                slow_client(addr, b"", b""),
                slow_client(
                    addr,
                    b"POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nContent-Length: 100000\r\n\r\n",
                    b"a",
                ),
            );
            let mut obs = Observations::new();
            for (label, outcome, timeout) in [
                ("slow head", slow_head?, HEADERS_TIMEOUT),
                ("nothing sent", silent?, HEADERS_TIMEOUT),
                ("slow body", slow_body?, REQUEST_TIMEOUT),
            ] {
                let (received, closed) = outcome;
                ensure!(
                    closed >= timeout && closed < timeout + Duration::from_millis(1500),
                    "{label}: closed after {closed:?}"
                );
                obs.record(format!("{label}: answer"), received);
                obs.record(format!("{label}: closed after the timeout"), true);
            }
            Ok(obs)
        },
    )
    .await;
}

/// REQ-29: a keep-alive response announces `Keep-Alive: timeout=<keepAliveTimeout in s>`;
/// a request sent within the timeout is served on the same connection, and a connection
/// left idle for `keepAliveTimeout` plus `keepAliveTimeoutBuffer` after a response is closed
/// without a response.
#[tokio::test]
async fn req_29_keep_alive_timeout() {
    run(
        "req_29_keep_alive_timeout",
        &["REQ-29"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c.server(server())).await?;
            let mut conn = conformance::client::RawConn::connect(gw.addr()).await?;
            let mut obs = Observations::new();
            for round in ["first", "second"] {
                conn.send(&Request::get("/svc/v1/users")).await?;
                let response = conn.read_response(false, Duration::from_secs(5)).await?;
                obs.record(format!("{round}: status"), response.status);
                obs.record(
                    format!("{round}: keep-alive"),
                    response.header("keep-alive").unwrap_or("(none)"),
                );
                if round == "first" {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                }
            }
            obs.record("requests on one connection", mock.requests().len());
            let idle = Instant::now();
            let rest = conn.read_to_close(Duration::from_secs(5)).await?;
            let closed = idle.elapsed();
            let expected = KEEP_ALIVE + KEEP_ALIVE_BUFFER;
            ensure!(
                closed >= expected - Duration::from_millis(100)
                    && closed < expected + Duration::from_millis(1000),
                "closed after {closed:?} idle"
            );
            obs.record("bytes before the close", rest.len());
            obs.record("closed after the keep-alive timeout", true);
            Ok(obs)
        },
    )
    .await;
}
