//! Mock Upstream Service for the end-to-end benchmark (`benches/README.md`).
//!
//! It answers fast enough not to be the bottleneck (the benchmark measures it directly to
//! prove that) and counts what it received, so the benchmark can check that every request
//! it reports really passed through a gateway:
//!
//! - `GET /openapi.json`: the benchmark document (`benches/fixtures/bench-service.json`).
//! - `GET /__stats`: `{"requests":N,"withUser":U,"withoutUser":A}`, where `withUser`
//!   counts requests carrying the `auth-user-id` header (set only by the benchmark
//!   middleware) and `withoutUser` the others. `POST /__stats/reset` zeroes them.
//! - Anything else is counted, then answered by shape:
//!   - a path containing `/export`: about 1 MiB of JSON;
//!   - `POST`: the body is read in full, then 201 with a small JSON body;
//!   - a path containing `/statics/`: 2 KiB of CSS;
//!   - otherwise: 200 with a small JSON body.
//!
//! Every answer carries the headers a NestJS service sends (`x-powered-by`, `etag`,
//! `content-type`, `content-length`, `date`).
//!
//! Environment: `LISTEN` (default `0.0.0.0:5000`), `THREADS` (default: all CPUs).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, ETAG, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

const DOCUMENT: &str = include_str!("../../fixtures/bench-service.json");
const SMALL: &[u8] = br#"{"id":"42","status":"shipped","total":129.5,"currency":"EUR","items":3,"updatedAt":"2026-11-18T08:00:00.000Z"}"#;
const CREATED: &[u8] = br#"{"id":"ord_7Hq2","status":"created"}"#;

#[derive(Default)]
struct Stats {
    with_user: AtomicU64,
    without_user: AtomicU64,
}

struct Bodies {
    large: Bytes,
    css: Bytes,
}

fn bodies() -> Bodies {
    // About 1 MiB: an array of 8,192 rows of 128 bytes each.
    let row = br#"{"id":"row-000000","name":"Quarterly export row","amount":1234.56,"currency":"EUR","note":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"},"#;
    let mut large = Vec::with_capacity(8192 * row.len() + 16);
    large.extend_from_slice(b"{\"rows\":[");
    for _ in 0..8192 {
        large.extend_from_slice(row);
    }
    large.pop();
    large.extend_from_slice(b"]}");
    let css = ".button{color:#123456;padding:4px 8px}\n".repeat(64);
    Bodies {
        large: Bytes::from(large),
        css: Bytes::from(css.into_bytes()[..2048].to_vec()),
    }
}

fn respond(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert("x-powered-by", HeaderValue::from_static("Express"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(ETAG, HeaderValue::from_static("W/\"6e-bench\""));
    response
}

const JSON: &str = "application/json; charset=utf-8";

async fn handle(
    request: Request<Incoming>,
    stats: Arc<Stats>,
    bodies: Arc<Bodies>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = request.uri().path().to_owned();
    match (request.method(), path.as_str()) {
        (&Method::GET, "/openapi.json") => {
            return Ok(respond(StatusCode::OK, JSON, Bytes::from_static(DOCUMENT.as_bytes())));
        }
        (&Method::GET, "/__stats") => {
            let with_user = stats.with_user.load(Ordering::SeqCst);
            let without_user = stats.without_user.load(Ordering::SeqCst);
            let body = format!(
                r#"{{"requests":{},"withUser":{with_user},"withoutUser":{without_user}}}"#,
                with_user + without_user
            );
            return Ok(respond(StatusCode::OK, JSON, Bytes::from(body)));
        }
        (&Method::POST, "/__stats/reset") => {
            stats.with_user.store(0, Ordering::SeqCst);
            stats.without_user.store(0, Ordering::SeqCst);
            return Ok(respond(StatusCode::OK, JSON, Bytes::from_static(b"{}")));
        }
        _ => {}
    }
    if request.headers().contains_key("auth-user-id") {
        stats.with_user.fetch_add(1, Ordering::Relaxed);
    } else {
        stats.without_user.fetch_add(1, Ordering::Relaxed);
    }
    let response = if path.contains("/export") {
        respond(StatusCode::OK, JSON, bodies.large.clone())
    } else if request.method() == Method::POST {
        // Read the whole body, as a NestJS body parser would.
        let _ = request.into_body().collect().await.map(|b| b.to_bytes());
        respond(StatusCode::CREATED, JSON, Bytes::from_static(CREATED))
    } else if path.contains("/statics/") {
        respond(StatusCode::OK, "text/css; charset=UTF-8", bodies.css.clone())
    } else {
        respond(StatusCode::OK, JSON, Bytes::from_static(SMALL))
    };
    Ok(response)
}

fn main() {
    let threads = std::env::var("THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let addr: SocketAddr = std::env::var("LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:5000".into())
        .parse()
        .expect("LISTEN is host:port");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let listener = TcpListener::bind(addr).await.expect("bind");
        eprintln!("bench-upstream listening on {addr} with {threads} threads");
        let stats = Arc::new(Stats::default());
        let bodies = Arc::new(bodies());
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            let stats = Arc::clone(&stats);
            let bodies = Arc::clone(&bodies);
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    handle(request, Arc::clone(&stats), Arc::clone(&bodies))
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}
