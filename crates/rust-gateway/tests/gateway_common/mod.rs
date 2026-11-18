//! Helpers for the Rust-only gateway tests: an in-process upstream, a gateway served on a
//! local port, and a plain HTTP client.

#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use rust_gateway::gateway::{Gateway, GatewayBuilder, GatewayOptions};
use rust_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
use rust_gateway::upstream::UpstreamService;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// The upstream's OpenAPI document.
pub fn document() -> Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "test", "version": "1" },
        "paths": {
            "/v1/users": {
                "get": { "operationId": "listUsers" },
                "post": { "operationId": "createUser" }
            },
            "/v1/limited": {
                "get": { "operationId": "limited", "x-rate-limits": [{ "limit": 1, "ttl": 60 }] }
            }
        }
    })
}

/// A request the upstream received.
#[derive(Debug, Clone)]
pub struct Seen {
    pub connection: u64,
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

/// An upstream serving [`document`] at `/openapi.json` and echoing every other request as
/// JSON, counting its connections.
pub struct Upstream {
    pub addr: SocketAddr,
    pub https: bool,
    seen: Arc<Mutex<Vec<Seen>>>,
    connections: Arc<AtomicU64>,
    _stop: oneshot::Sender<()>,
}

impl Upstream {
    pub async fn start() -> Self {
        Self::start_with(None).await
    }

    /// An upstream speaking TLS with this server configuration.
    pub async fn start_with(tls: Option<Arc<rustls::ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicU64::new(0));
        let (stop, mut stopped) = oneshot::channel::<()>();
        let https = tls.is_some();
        let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
        {
            let seen = Arc::clone(&seen);
            let connections = Arc::clone(&connections);
            tokio::spawn(async move {
                loop {
                    let (stream, _) = tokio::select! {
                        _ = &mut stopped => return,
                        accepted = listener.accept() => match accepted {
                            Ok(accepted) => accepted,
                            Err(_) => return,
                        },
                    };
                    let id = connections.fetch_add(1, Ordering::SeqCst) + 1;
                    let seen = Arc::clone(&seen);
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |request| {
                            let seen = Arc::clone(&seen);
                            async move { Ok::<_, Infallible>(answer(request, id, &seen).await) }
                        });
                        let builder = hyper::server::conn::http1::Builder::new();
                        match acceptor {
                            Some(acceptor) => {
                                if let Ok(tls) = acceptor.accept(stream).await {
                                    let _ =
                                        builder.serve_connection(TokioIo::new(tls), service).await;
                                }
                            }
                            None => {
                                let _ = builder
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            }
                        }
                    });
                }
            });
        }
        Self {
            addr,
            https,
            seen,
            connections,
            _stop: stop,
        }
    }

    pub fn origin(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        let host = if self.https { "localhost" } else { "127.0.0.1" };
        format!("{scheme}://{host}:{}", self.addr.port())
    }

    pub fn service(&self, prefix: &str) -> UpstreamService {
        UpstreamService::new(
            prefix,
            format!("http://127.0.0.1:{}/openapi.json", self.document_port()),
            self.origin(),
        )
    }

    /// Documents are always fetched over plain HTTP; a TLS upstream serves its document
    /// through [`Upstream::document_server`].
    fn document_port(&self) -> u16 {
        self.addr.port()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }

    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::SeqCst)
    }
}

async fn answer(
    request: Request<Incoming>,
    connection: u64,
    seen: &Mutex<Vec<Seen>>,
) -> Response<Full<Bytes>> {
    let target = request
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    if target == "/openapi.json" {
        return Response::new(Full::new(Bytes::from(document().to_string())));
    }
    let entry = Seen {
        connection,
        method: request.method().to_string(),
        target: target.clone(),
        headers: request
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
    };
    seen.lock().expect("seen").push(entry);
    let delay = target
        .split("delay=")
        .nth(1)
        .and_then(|ms| ms.split('&').next())
        .and_then(|ms| ms.parse::<u64>().ok());
    let _ = request.into_body().collect().await;
    if let Some(ms) = delay {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let mut response = Response::new(Full::new(Bytes::from(format!("upstream {target}"))));
    // `powered=<value>` makes the upstream send its own `x-powered-by` (FWD-15).
    if let Some(value) = target
        .split("powered=")
        .nth(1)
        .and_then(|v| v.split('&').next())
        .and_then(|v| http::HeaderValue::from_str(v).ok())
    {
        response.headers_mut().insert("x-powered-by", value);
    }
    response
}

/// Sends `head` (a complete request head) on a new connection that the gateway closes after
/// answering, and returns the status line and the header lines in wire order, names
/// lower-cased.
pub async fn raw_exchange(addr: SocketAddr, head: &str) -> (String, Vec<(String, String)>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream.write_all(head.as_bytes()).await.expect("send");
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut answer)).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers)
}

/// Gateway options for one service, with rate limiting off (no Redis needed).
pub fn options(services: Vec<UpstreamService>) -> GatewayOptions {
    let mut options = GatewayOptions::new(
        services,
        RateLimitOptions::new(
            false,
            BucketLimit::new(1000, 60.0),
            BucketLimit::new(1000, 60.0),
        ),
        RedisOptions::new("127.0.0.1", 1),
    );
    options.exclude_headers = vec!["auth-user-id".into()];
    options
}

/// A gateway served on a local port. Dropping it stops the server.
pub struct Served {
    pub addr: SocketAddr,
    pub gateway: Gateway,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Served {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// Stops the server and waits for it to return.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.task.take() {
            Some(task) => task.await.expect("server task"),
            None => Ok(()),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Builds the gateway, serves it on the IPv4 loopback, and (unless `wait` is false) waits
/// for its documents.
pub async fn serve(builder: GatewayBuilder, wait: bool) -> Served {
    let gateway = builder.build().expect("gateway builds");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway");
    let addr = listener.local_addr().expect("gateway address");
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(gateway.clone().serve(listener, async move {
        let _ = stopped.await;
    }));
    if wait {
        tokio::time::timeout(Duration::from_secs(10), gateway.ready())
            .await
            .expect("documents load");
    }
    Served {
        addr,
        gateway,
        stop: Some(stop),
        task: Some(task),
    }
}

/// A client that never pools, so every request is its own connection.
///
/// It only speaks plain `http` to the gateway, so its TLS settings trust no roots: building
/// it reads nothing from the host's trust store, which the child process of
/// `gateway_forwarding::startup_needs_no_system_trust_store` empties on Linux (a client
/// verifying with the platform's roots fails to build there).
pub fn client() -> reqwest::Client {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .tls_backend_preconfigured(tls)
        .build()
        .expect("client")
}
