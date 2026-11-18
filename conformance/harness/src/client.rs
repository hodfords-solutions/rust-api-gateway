//! A raw HTTP/1.1 client for talking to a gateway target.
//!
//! Requests are written byte for byte as built, so scenarios can send request targets and
//! header sets that a high-level client would normalise or refuse. [`RawConn`] gives
//! socket-level access for WebSocket and malformed-request cases.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::wire::{Body, Headers, WireReader, header, header_all, parse_response_head};

/// Default time a scenario waits for a response.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// A request to be written verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub version: &'static str,
    /// Sent in this order. A `Host` header is added first unless one is present.
    pub headers: Headers,
    pub body: Vec<u8>,
    /// Adds `content-length` when the body is non-empty and no framing header was given.
    pub auto_length: bool,
}

impl Request {
    pub fn new(method: &str, target: &str) -> Self {
        Self {
            method: method.to_string(),
            target: target.to_string(),
            version: "HTTP/1.1",
            headers: Vec::new(),
            body: Vec::new(),
            auto_length: true,
        }
    }

    pub fn get(target: &str) -> Self {
        Self::new("GET", target)
    }

    pub fn post(target: &str) -> Self {
        Self::new("POST", target)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn json(self, value: &serde_json::Value) -> Self {
        self.header("content-type", "application/json")
            .body(value.to_string())
    }

    pub fn to_bytes(&self, addr: SocketAddr) -> Vec<u8> {
        let mut out = format!("{} {} {}\r\n", self.method, self.target, self.version);
        if header(&self.headers, "host").is_none() {
            out.push_str(&format!("Host: {addr}\r\n"));
        }
        for (n, v) in &self.headers {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        let framed = header(&self.headers, "content-length").is_some()
            || header(&self.headers, "transfer-encoding").is_some();
        if self.auto_length && !framed && !self.body.is_empty() {
            out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }
}

/// A response as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub version: u8,
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
    pub body: Vec<u8>,
    /// Whether the body's framing completed (false for a cut-short response).
    pub complete: bool,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.headers, name)
    }

    pub fn header_all(&self, name: &str) -> Vec<&str> {
        header_all(&self.headers, name)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body)
            .with_context(|| format!("response body is not JSON: {:?}", self.text()))
    }
}

/// Outcome of waiting for bytes on a raw connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    Data(Vec<u8>),
    /// The peer closed (or reset) the connection.
    Closed,
    /// Nothing arrived before the deadline; the connection is still open.
    Timeout,
}

/// A raw TCP connection to a target.
pub struct RawConn {
    addr: SocketAddr,
    reader: WireReader<TcpStream>,
}

impl RawConn {
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .await
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_nodelay(true)?;
        Ok(Self {
            addr,
            reader: WireReader::new(stream),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.reader.get_mut().write_all(bytes).await?;
        Ok(())
    }

    pub async fn send(&mut self, request: &Request) -> Result<()> {
        let bytes = request.to_bytes(self.addr);
        self.write(&bytes).await
    }

    /// Reads one response. `is_head` suppresses the body for a `HEAD` request.
    pub async fn read_response(&mut self, is_head: bool, timeout: Duration) -> Result<Response> {
        tokio::time::timeout(timeout, self.read_response_inner(is_head))
            .await
            .map_err(|_| anyhow!("no complete response head within {timeout:?}"))?
    }

    async fn read_response_inner(&mut self, is_head: bool) -> Result<Response> {
        let head_bytes = self
            .reader
            .read_head()
            .await?
            .ok_or_else(|| anyhow!("connection closed before any response"))?;
        let head = parse_response_head(&head_bytes)?;
        let bodyless = is_head || matches!(head.status, 100..=199 | 204 | 304);
        let body = if bodyless {
            Body {
                bytes: Vec::new(),
                complete: true,
            }
        } else if header(&head.headers, "transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            self.reader.read_chunked().await?
        } else if let Some(len) = header(&head.headers, "content-length") {
            self.reader.read_sized(len.trim().parse()?).await?
        } else {
            Body {
                bytes: self.reader.read_to_eof().await?,
                complete: true,
            }
        };
        Ok(Response {
            version: head.version,
            status: head.status,
            reason: head.reason,
            headers: head.headers,
            body: body.bytes,
            complete: body.complete,
        })
    }

    /// Waits up to `timeout` for whatever arrives next.
    pub async fn read_some(&mut self, timeout: Duration) -> ReadOutcome {
        let buffered = self.reader.take_buffered();
        if !buffered.is_empty() {
            return ReadOutcome::Data(buffered);
        }
        read_raw(self.reader.get_mut(), timeout).await
    }

    /// Returns the stream and any bytes already read past the last response.
    pub fn into_parts(self) -> (TcpStream, Vec<u8>) {
        self.reader.into_parts()
    }
}

/// Sends one request on a fresh connection and reads the response.
pub async fn send(addr: SocketAddr, request: &Request) -> Result<Response> {
    send_with_timeout(addr, request, DEFAULT_TIMEOUT).await
}

pub async fn send_with_timeout(
    addr: SocketAddr,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let mut conn = RawConn::connect(addr).await?;
    conn.send(request).await?;
    conn.read_response(request.method.eq_ignore_ascii_case("HEAD"), timeout)
        .await
}

/// A client-side WebSocket upgrade: the handshake response, and the socket when it upgraded.
pub struct Upgrade {
    pub response_head: Response,
    pub socket: Option<WebSocketStream<TcpStream>>,
}

/// Builds a WebSocket upgrade request for `target`.
pub fn upgrade_request(target: &str) -> Request {
    Request::get(target)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
}

/// Sends an upgrade request and, on 101, wraps the socket as a client WebSocket.
pub async fn websocket(addr: SocketAddr, request: &Request) -> Result<Upgrade> {
    let mut conn = RawConn::connect(addr).await?;
    conn.send(request).await?;
    let head_bytes = tokio::time::timeout(DEFAULT_TIMEOUT, conn.reader.read_head())
        .await
        .map_err(|_| anyhow!("no upgrade response"))??
        .ok_or_else(|| anyhow!("connection closed before the upgrade response"))?;
    let head = parse_response_head(&head_bytes)?;
    let response_head = Response {
        version: head.version,
        status: head.status,
        reason: head.reason,
        headers: head.headers,
        body: Vec::new(),
        complete: true,
    };
    if response_head.status != 101 {
        return Ok(Upgrade {
            response_head,
            socket: None,
        });
    }
    let (stream, leftover) = conn.into_parts();
    let socket = WebSocketStream::from_partially_read(stream, leftover, Role::Client, None).await;
    Ok(Upgrade {
        response_head,
        socket: Some(socket),
    })
}

/// Reads raw bytes from a stream until `timeout`, reporting a close.
pub async fn read_raw(stream: &mut TcpStream, timeout: Duration) -> ReadOutcome {
    let mut buf = [0u8; 16 * 1024];
    match tokio::time::timeout(timeout, stream.read(&mut buf)).await {
        Err(_) => ReadOutcome::Timeout,
        Ok(Ok(0)) | Ok(Err(_)) => ReadOutcome::Closed,
        Ok(Ok(n)) => ReadOutcome::Data(buf[..n].to_vec()),
    }
}

/// Fails unless `outcome` is a timeout (the connection stayed open and silent).
pub fn expect_silent(outcome: &ReadOutcome) -> Result<()> {
    match outcome {
        ReadOutcome::Timeout => Ok(()),
        other => bail!("expected an open, silent connection, got {other:?}"),
    }
}
