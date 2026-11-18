//! A controllable Upstream Service for scenarios.
//!
//! It serves an OpenAPI document at [`DOC_PATH`] (swappable at runtime), records every other
//! request exactly as received, and answers according to scripted [`Behaviour`]s chosen by
//! path prefix. It speaks HTTP/1.1 on raw TCP so that it can misbehave on purpose.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::AbortHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::wire::{Headers, WireReader, header, header_all, parse_request_head};

/// Path at which the mock serves its OpenAPI document.
pub const DOC_PATH: &str = "/openapi.json";

/// One proxied request as the upstream received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordedRequest {
    /// Accept-order number of the TCP connection it arrived on (starts at 1).
    pub connection: u64,
    pub method: String,
    /// Raw request target: path and query exactly as sent.
    pub target: String,
    /// Minor HTTP version (1 for HTTP/1.1).
    pub version: u8,
    /// Every header in order, with duplicates and original name case.
    pub headers: Headers,
    #[serde(serialize_with = "lossy_utf8")]
    pub body: Vec<u8>,
    /// Whether the body's framing completed.
    pub body_complete: bool,
}

fn lossy_utf8<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&String::from_utf8_lossy(bytes))
}

impl RecordedRequest {
    /// The path part of the target.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    /// The raw query string after `?`, if the target has one.
    pub fn query(&self) -> Option<&str> {
        self.target.split_once('?').map(|(_, q)| q)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    pub fn header_all(&self, name: &str) -> Vec<&str> {
        header_all(&self.headers, name)
    }

    /// Headers with lower-cased names, minus the `x-test-*` protocol headers.
    pub fn headers_lower(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .filter(|(n, _)| !n.starts_with("x-test-"))
            .collect()
    }
}

/// A fixed response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub reason: Option<String>,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            reason: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status)
            .header("content-type", "application/json")
            .body(value.to_string())
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }
}

/// What the mock does with a proxied request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Behaviour {
    /// 200 with the [`RecordedRequest`] as JSON. The default.
    Echo,
    Respond(Reply),
    /// Waits, then behaves as the inner behaviour.
    Delay(Duration, Box<Behaviour>),
    /// Sends `count` copies of `chunk`, sleeping `delay` before each. Chunked unless
    /// `headers` contains a `content-length`.
    Stream {
        status: u16,
        headers: Headers,
        chunk: Vec<u8>,
        count: usize,
        delay: Duration,
    },
    /// Closes the connection without answering.
    CloseBeforeHeaders,
    /// Sends the head and `body` (raw bytes, already framed as the headers say), then closes.
    CloseMidBody {
        status: u16,
        headers: Headers,
        body: Vec<u8>,
    },
    /// Keeps the connection open and never answers.
    Hang,
    /// Accepts a WebSocket upgrade and echoes every text and binary message.
    WebSocketEcho,
    /// Accepts a WebSocket upgrade, sends this text frame in the same write as the 101
    /// response, then echoes.
    WebSocketSpeakFirst(String),
    /// Answers an upgrade request without upgrading, then closes.
    RejectUpgrade {
        status: u16,
        reason: String,
        body: Vec<u8>,
        content_length: bool,
    },
}

impl Behaviour {
    pub fn redirect(status: u16, location: &str) -> Self {
        Behaviour::Respond(Reply::new(status).header("location", location))
    }
}

/// What the mock answers at [`DOC_PATH`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Document {
    Json(Vec<u8>),
    Status(u16),
    Close,
}

struct State {
    document: Mutex<Document>,
    rules: Mutex<Vec<(String, Behaviour)>>,
    default: Mutex<Behaviour>,
    recorded: Mutex<Vec<RecordedRequest>>,
    recorded_changed: Notify,
    connections: AtomicU64,
    document_fetches: AtomicU64,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// A running mock upstream. Dropping it stops the server and closes its connections.
pub struct MockUpstream {
    addr: SocketAddr,
    state: Arc<State>,
}

impl MockUpstream {
    /// Starts on an ephemeral port on 127.0.0.1, serving a 404 document until one is set.
    pub async fn start() -> Result<Self> {
        Self::start_on("127.0.0.1:0".parse()?).await
    }

    pub async fn start_on(addr: SocketAddr) -> Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding mock upstream on {addr}"))?;
        let addr = listener.local_addr()?;
        let state = Arc::new(State {
            document: Mutex::new(Document::Status(404)),
            rules: Mutex::new(Vec::new()),
            default: Mutex::new(Behaviour::Echo),
            recorded: Mutex::new(Vec::new()),
            recorded_changed: Notify::new(),
            connections: AtomicU64::new(0),
            document_fetches: AtomicU64::new(0),
            tasks: Mutex::new(Vec::new()),
        });
        let accept_state = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let id = accept_state.connections.fetch_add(1, Ordering::SeqCst) + 1;
                let conn_state = accept_state.clone();
                let task = tokio::spawn(async move {
                    let _ = serve_connection(stream, id, conn_state).await;
                });
                accept_state.tasks.lock().unwrap().push(task.abort_handle());
            }
        });
        state.tasks.lock().unwrap().push(accept.abort_handle());
        Ok(Self { addr, state })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://127.0.0.1:<port>`, the value for an Upstream Service's `host`.
    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The value for an Upstream Service's `docUrl`.
    pub fn doc_url(&self) -> String {
        format!("{}{DOC_PATH}", self.origin())
    }

    pub fn set_document(&self, document: Document) {
        *self.state.document.lock().unwrap() = document;
    }

    pub fn set_document_json(&self, json: &str) {
        self.set_document(Document::Json(json.as_bytes().to_vec()));
    }

    /// Requests whose path starts with `prefix` get `behaviour`. Later rules win.
    pub fn route(&self, prefix: &str, behaviour: Behaviour) {
        self.state
            .rules
            .lock()
            .unwrap()
            .push((prefix.to_string(), behaviour));
    }

    /// Behaviour for requests no rule matches (initially [`Behaviour::Echo`]).
    pub fn set_default(&self, behaviour: Behaviour) {
        *self.state.default.lock().unwrap() = behaviour;
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.recorded.lock().unwrap().clone()
    }

    pub fn clear_requests(&self) {
        self.state.recorded.lock().unwrap().clear();
    }

    /// Waits until at least `count` requests were recorded.
    pub async fn wait_for_requests(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<RecordedRequest>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.state.recorded_changed.notified();
            let current = self.requests();
            if current.len() >= count {
                return Ok(current);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                bail!(
                    "mock upstream {}: expected {count} requests, got {}",
                    self.addr,
                    current.len()
                );
            }
        }
    }

    /// The most recent recorded request.
    pub fn last_request(&self) -> Option<RecordedRequest> {
        self.requests().pop()
    }

    /// Number of times the document was fetched.
    pub fn document_fetches(&self) -> u64 {
        self.state.document_fetches.load(Ordering::SeqCst)
    }

    /// Number of TCP connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.state.connections.load(Ordering::SeqCst)
    }
}

impl Drop for MockUpstream {
    fn drop(&mut self) {
        for task in self.state.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}

fn choose(state: &State, path: &str) -> Behaviour {
    let rules = state.rules.lock().unwrap();
    rules
        .iter()
        .rev()
        .find(|(prefix, _)| path.starts_with(prefix.as_str()))
        .map(|(_, b)| b.clone())
        .unwrap_or_else(|| state.default.lock().unwrap().clone())
}

/// What happens to the connection after a behaviour ran.
enum After {
    KeepAlive,
    Close,
}

async fn serve_connection(stream: TcpStream, id: u64, state: Arc<State>) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = WireReader::new(stream);
    loop {
        let Some(head_bytes) = reader.read_head().await? else {
            return Ok(());
        };
        let head = parse_request_head(&head_bytes)?;
        let path = head.target.split('?').next().unwrap_or("").to_string();
        let wants_close = header(&head.headers, "connection")
            .is_some_and(|v| v.eq_ignore_ascii_case("close"))
            || head.version == 0;

        if path == DOC_PATH {
            let _ = reader.read_request_body(&head.headers).await?;
            state.document_fetches.fetch_add(1, Ordering::SeqCst);
            let document = state.document.lock().unwrap().clone();
            let reply = match document {
                Document::Json(json) => Reply::new(200)
                    .header("content-type", "application/json; charset=utf-8")
                    .body(json),
                Document::Status(status) => Reply::new(status)
                    .header("content-type", "application/json")
                    .body(format!("{{\"statusCode\":{status}}}")),
                Document::Close => return Ok(()),
            };
            write_reply(reader.get_mut(), &reply, false).await?;
            if wants_close {
                return Ok(());
            }
            continue;
        }

        let body = reader.read_request_body(&head.headers).await?;
        let recorded = RecordedRequest {
            connection: id,
            method: head.method.clone(),
            target: head.target.clone(),
            version: head.version,
            headers: head.headers.clone(),
            body: body.bytes,
            body_complete: body.complete,
        };
        state.recorded.lock().unwrap().push(recorded.clone());
        state.recorded_changed.notify_waiters();

        let behaviour = choose(&state, &path);
        match behaviour {
            Behaviour::WebSocketEcho => return websocket(reader, &recorded, None).await,
            Behaviour::WebSocketSpeakFirst(text) => {
                return websocket(reader, &recorded, Some(text)).await;
            }
            _ => {}
        }
        let is_head = head.method.eq_ignore_ascii_case("HEAD");
        match run_behaviour(&mut reader, behaviour, &recorded, is_head).await? {
            After::KeepAlive if !wants_close => continue,
            _ => {
                let _ = reader.get_mut().shutdown().await;
                return Ok(());
            }
        }
    }
}

async fn run_behaviour(
    reader: &mut WireReader<TcpStream>,
    mut behaviour: Behaviour,
    request: &RecordedRequest,
    is_head: bool,
) -> Result<After> {
    loop {
        match behaviour {
            Behaviour::Delay(delay, inner) => {
                tokio::time::sleep(delay).await;
                behaviour = *inner;
            }
            Behaviour::Echo => {
                let reply = Reply::json(200, &serde_json::to_value(request)?);
                write_reply(reader.get_mut(), &reply, is_head).await?;
                return Ok(After::KeepAlive);
            }
            Behaviour::Respond(reply) => {
                write_reply(reader.get_mut(), &reply, is_head).await?;
                return Ok(After::KeepAlive);
            }
            Behaviour::Stream {
                status,
                headers,
                chunk,
                count,
                delay,
            } => {
                let sized = header(&headers, "content-length").is_some();
                let mut head = status_line(status, None);
                for (n, v) in &headers {
                    head.push_str(&format!("{n}: {v}\r\n"));
                }
                if !sized {
                    head.push_str("transfer-encoding: chunked\r\n");
                }
                head.push_str("\r\n");
                let stream = reader.get_mut();
                stream.write_all(head.as_bytes()).await?;
                for _ in 0..count {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    if sized {
                        stream.write_all(&chunk).await?;
                    } else {
                        stream
                            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                            .await?;
                        stream.write_all(&chunk).await?;
                        stream.write_all(b"\r\n").await?;
                    }
                }
                if !sized {
                    stream.write_all(b"0\r\n\r\n").await?;
                }
                return Ok(After::KeepAlive);
            }
            Behaviour::CloseBeforeHeaders => return Ok(After::Close),
            Behaviour::CloseMidBody {
                status,
                headers,
                body,
            } => {
                let mut head = status_line(status, None);
                for (n, v) in &headers {
                    head.push_str(&format!("{n}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let stream = reader.get_mut();
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&body).await?;
                stream.flush().await?;
                // Give the bytes a moment to leave before the connection drops.
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Ok(After::Close);
            }
            Behaviour::Hang => {
                std::future::pending::<()>().await;
            }
            Behaviour::RejectUpgrade {
                status,
                reason,
                body,
                content_length,
            } => {
                let mut head = status_line(status, Some(&reason));
                head.push_str("x-mock-rejected: 1\r\n");
                if content_length {
                    head.push_str(&format!("content-length: {}\r\n", body.len()));
                } else {
                    head.push_str("connection: close\r\n");
                }
                head.push_str("\r\n");
                let stream = reader.get_mut();
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(&body).await?;
                stream.flush().await?;
                return Ok(After::Close);
            }
            // Handled by `serve_connection`, which owns the connection.
            Behaviour::WebSocketEcho | Behaviour::WebSocketSpeakFirst(_) => {
                return Ok(After::Close);
            }
        }
    }
}

fn status_line(status: u16, reason: Option<&str>) -> String {
    format!(
        "HTTP/1.1 {status} {}\r\n",
        reason.unwrap_or_else(|| default_reason(status))
    )
}

fn default_reason(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn write_reply(stream: &mut TcpStream, reply: &Reply, is_head: bool) -> Result<()> {
    let mut head = status_line(reply.status, reply.reason.as_deref());
    let has_length = header(&reply.headers, "content-length").is_some()
        || header(&reply.headers, "transfer-encoding").is_some();
    for (n, v) in &reply.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    let bodyless = matches!(reply.status, 100..=199 | 204 | 304);
    if !has_length && !bodyless {
        head.push_str(&format!("content-length: {}\r\n", reply.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    if !is_head && !bodyless {
        stream.write_all(&reply.body).await?;
    }
    stream.flush().await?;
    Ok(())
}

async fn websocket(
    mut reader: WireReader<TcpStream>,
    request: &RecordedRequest,
    first: Option<String>,
) -> Result<()> {
    let key = request
        .header("sec-websocket-key")
        .context("upgrade request without sec-websocket-key")?;
    let mut out = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nx-mock-upstream: ws\r\n\r\n",
        derive_accept_key(key.as_bytes())
    )
    .into_bytes();
    if let Some(text) = &first {
        out.extend_from_slice(&text_frame(text));
    }
    reader.get_mut().write_all(&out).await?;

    // Keep any bytes the client sent along with its handshake.
    let (stream, leftover) = reader.into_parts();
    let mut ws = WebSocketStream::from_partially_read(stream, leftover, Role::Server, None).await;
    while let Some(message) = ws.next().await {
        match message? {
            m @ (Message::Text(_) | Message::Binary(_)) => ws.send(m).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// An unmasked server-to-client text frame.
fn text_frame(text: &str) -> Vec<u8> {
    let payload = text.as_bytes();
    let mut frame = vec![0x81];
    match payload.len() {
        n if n < 126 => frame.push(n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(126);
            frame.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            frame.push(127);
            frame.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{self, ReadOutcome, Request, read_raw};

    #[tokio::test]
    async fn records_requests_exactly_and_serves_the_document() {
        let mock = MockUpstream::start().await.unwrap();
        mock.set_document_json(r#"{"paths":{}}"#);
        let doc = client::send(mock.addr(), &Request::get(DOC_PATH))
            .await
            .unwrap();
        assert_eq!(doc.text(), r#"{"paths":{}}"#);
        assert_eq!(mock.document_fetches(), 1);

        let request = Request::post("/a/b?x=1&x=2&y")
            .header("X-Dup", "1")
            .header("x-dup", "2")
            .body("hello");
        let response = client::send(mock.addr(), &request).await.unwrap();
        assert_eq!(response.status, 200);
        let recorded = mock.last_request().unwrap();
        assert_eq!(recorded.method, "POST");
        assert_eq!(recorded.path(), "/a/b");
        assert_eq!(recorded.query(), Some("x=1&x=2&y"));
        assert_eq!(recorded.header_all("x-dup"), vec!["1", "2"]);
        assert_eq!(recorded.body, b"hello");
        assert_eq!(
            mock.requests().len(),
            1,
            "document fetches are not recorded"
        );
        let echoed = response.json().unwrap();
        assert_eq!(echoed["target"], "/a/b?x=1&x=2&y");
    }

    #[tokio::test]
    async fn scripted_behaviours() {
        let mock = MockUpstream::start().await.unwrap();
        mock.route("/redirect", Behaviour::redirect(302, "/login"));
        mock.route(
            "/stream",
            Behaviour::Stream {
                status: 200,
                headers: vec![],
                chunk: b"abc".to_vec(),
                count: 3,
                delay: Duration::ZERO,
            },
        );
        mock.route(
            "/cut",
            Behaviour::CloseMidBody {
                status: 200,
                headers: vec![("content-length".into(), "100".into())],
                body: b"0123456789".to_vec(),
            },
        );
        mock.route("/close", Behaviour::CloseBeforeHeaders);

        let r = client::send(mock.addr(), &Request::get("/redirect"))
            .await
            .unwrap();
        assert_eq!((r.status, r.header("location")), (302, Some("/login")));

        let r = client::send(mock.addr(), &Request::get("/stream"))
            .await
            .unwrap();
        assert_eq!(r.body, b"abcabcabc");
        assert!(r.complete);

        let r = client::send(mock.addr(), &Request::get("/cut"))
            .await
            .unwrap();
        assert_eq!(r.body, b"0123456789");
        assert!(!r.complete);

        assert!(
            client::send(mock.addr(), &Request::get("/close"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn keeps_connections_alive_and_numbers_them() {
        let mock = MockUpstream::start().await.unwrap();
        let mut conn = client::RawConn::connect(mock.addr()).await.unwrap();
        for _ in 0..2 {
            conn.send(&Request::get("/x")).await.unwrap();
            conn.read_response(false, client::DEFAULT_TIMEOUT)
                .await
                .unwrap();
        }
        client::send(mock.addr(), &Request::get("/y"))
            .await
            .unwrap();
        let ids: Vec<u64> = mock.requests().iter().map(|r| r.connection).collect();
        assert_eq!(ids, vec![1, 1, 2]);
    }

    #[tokio::test]
    async fn websocket_behaviours() {
        let mock = MockUpstream::start().await.unwrap();
        mock.route("/ws", Behaviour::WebSocketEcho);
        mock.route("/first", Behaviour::WebSocketSpeakFirst("hello".into()));
        mock.route(
            "/reject",
            Behaviour::RejectUpgrade {
                status: 401,
                reason: "Unauthorized".into(),
                body: b"no".to_vec(),
                content_length: false,
            },
        );

        let upgrade = client::websocket(mock.addr(), &client::upgrade_request("/ws"))
            .await
            .unwrap();
        assert_eq!(upgrade.response_head.status, 101);
        let mut ws = upgrade.socket.unwrap();
        ws.send(Message::text("ping")).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("ping"));

        let upgrade = client::websocket(mock.addr(), &client::upgrade_request("/first"))
            .await
            .unwrap();
        let mut ws = upgrade.socket.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap(), Message::text("hello"));

        let mut conn = client::RawConn::connect(mock.addr()).await.unwrap();
        conn.send(&client::upgrade_request("/reject"))
            .await
            .unwrap();
        let r = conn
            .read_response(false, client::DEFAULT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (401, &b"no"[..]));
        let (mut stream, _) = conn.into_parts();
        assert_eq!(
            read_raw(&mut stream, Duration::from_secs(2)).await,
            ReadOutcome::Closed
        );
    }
}
