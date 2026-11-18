//! A raw upstream for the WebSocket tests: it serves the OpenAPI document, accepts every
//! other request as an upgrade (`101`), then echoes bytes until the gateway closes. It
//! counts the tunnels that are open on its side.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// The echoing upstream.
pub struct EchoUpstream {
    pub addr: SocketAddr,
    open: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    changed: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl EchoUpstream {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream address");
        let open = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        let changed = Arc::new(Notify::new());
        let task = {
            let (open, accepted, changed) = (open.clone(), accepted.clone(), changed.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (open, accepted, changed) =
                        (open.clone(), accepted.clone(), changed.clone());
                    tokio::spawn(async move {
                        serve(stream, &open, &accepted, &changed).await;
                    });
                }
            })
        };
        Self {
            addr,
            open,
            accepted,
            changed,
            task,
        }
    }

    /// Tunnels open on the upstream side.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// Upgrade requests accepted so far.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Waits until at most `count` tunnels are open; returns the number open at the end.
    pub async fn wait_open_at_most(&self, count: usize, timeout: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            let open = self.open();
            if open <= count || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.open();
            }
        }
    }
}

impl Drop for EchoUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn document() -> String {
    serde_json::json!({
        "openapi": "3.0.0",
        "info": { "title": "ws", "version": "1" },
        "paths": { "/v1/users": { "get": { "operationId": "listUsers" } } }
    })
    .to_string()
}

async fn serve(
    mut stream: TcpStream,
    open: &AtomicUsize,
    accepted: &AtomicUsize,
    changed: &Notify,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    if head.starts_with(b"GET /openapi.json ") {
        let body = document();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return;
    }
    accepted.fetch_add(1, Ordering::SeqCst);
    open.fetch_add(1, Ordering::SeqCst);
    changed.notify_waiters();
    if stream
        .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
        .await
        .is_ok()
    {
        let (mut read, mut write) = stream.split();
        let _ = tokio::io::copy(&mut read, &mut write).await;
    }
    open.fetch_sub(1, Ordering::SeqCst);
    changed.notify_waiters();
}

/// An upgrade request for `target`.
pub fn upgrade_request(target: &str, extra: &str) -> Vec<u8> {
    format!(
        "GET {target} HTTP/1.1\r\nHost: gw\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{extra}\r\n"
    )
    .into_bytes()
}

/// Reads one response head.
pub async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut byte))
            .await
            .expect("response head in time")
            .expect("read");
        assert_eq!(
            n,
            1,
            "connection closed inside a head: {:?}",
            String::from_utf8_lossy(&head)
        );
        head.push(byte[0]);
    }
    head
}

/// Reads until the gateway closes the connection.
pub async fn read_to_close(stream: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut out))
        .await
        .expect("the gateway closes the connection")
        .expect("read");
    out
}
