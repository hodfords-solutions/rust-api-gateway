//! The connection pool of one Upstream Service (CFG-10, FWD-19, FWD-20).
//!
//! It reproduces what is observable of undici's `Pool`, which the Reference Gateway creates
//! per Upstream Service:
//!
//! - connections are reused across sequential requests (FWD-20);
//! - a connection idle for longer than `keepAliveTimeout` (4 s by default) is not reused;
//! - `connections` caps the connections open at a time; further requests wait for one;
//! - `headersTimeout` and `bodyTimeout` (300 s by default) end a silent upstream;
//! - `http` and `https` origins (FWD-19).
//!
//! Requests are written with hyper's HTTP/1.1 client connection, which sends the request
//! head exactly as given: the pipeline builds the header list in undici's order.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future as _;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http::{Request, Response, Uri};
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::gateway::PoolOptions;
use crate::handler::BoxError;

use super::body::UpstreamRequestBody;

/// The request body type the pool sends.
#[cfg_attr(not(feature = "mcp"), allow(unused_imports))]
pub(crate) use super::body::UpstreamRequestBody as RequestBody;

/// Why an upstream request produced no response.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PoolError {
    #[error("connecting to the upstream failed: {0}")]
    Connect(#[source] std::io::Error),
    #[error("connecting to the upstream timed out")]
    ConnectTimeout,
    #[error("TLS with the upstream failed: {0}")]
    Tls(String),
    #[error("the upstream request failed: {0}")]
    Http(#[from] hyper::Error),
    #[error("the upstream sent no response headers in time")]
    HeadersTimeout,
}

impl PoolError {
    /// The failure's `error.type` on a client span: the code undici gives it.
    #[cfg(feature = "otel")]
    pub(crate) fn trace_type(&self) -> &'static str {
        match self {
            Self::Connect(error) => match crate::otel::io_error_type(error) {
                "Error" => "UND_ERR_CONNECT",
                code => code,
            },
            Self::ConnectTimeout => "UND_ERR_CONNECT_TIMEOUT",
            Self::Tls(_) => "ERR_TLS",
            Self::Http(_) => "UND_ERR_SOCKET",
            Self::HeadersTimeout => "UND_ERR_HEADERS_TIMEOUT",
        }
    }
}

/// Where an Upstream Service is reached.
#[derive(Debug, Clone)]
pub(crate) struct Origin {
    https: bool,
    host: String,
    port: u16,
    /// `host[:port]` as written in the configured origin, for a `Host` header when the client
    /// sent none.
    authority: String,
}

impl Origin {
    /// Parses an Upstream Service's `host` (an origin such as `http://127.0.0.1:2009`).
    pub(crate) fn parse(origin: &str) -> Result<Self, String> {
        let uri: Uri = origin
            .parse()
            .map_err(|e| format!("invalid upstream origin {origin:?}: {e}"))?;
        let https = match uri.scheme_str() {
            Some("http") => false,
            Some("https") => true,
            _ => return Err(format!("upstream origin {origin:?} must be http or https")),
        };
        let authority = uri
            .authority()
            .ok_or_else(|| format!("upstream origin {origin:?} has no host"))?;
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = authority.port_u16().unwrap_or(if https { 443 } else { 80 });
        Ok(Self {
            https,
            host,
            port,
            authority: authority.as_str().to_string(),
        })
    }

    pub(crate) fn authority(&self) -> &str {
        &self.authority
    }

    /// `http` or `https`.
    #[cfg(all(feature = "otel", feature = "mcp"))]
    pub(crate) fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }
}

struct Idle {
    sender: SendRequest<UpstreamRequestBody>,
    since: Instant,
}

/// The pool of one Upstream Service.
pub(crate) struct UpstreamPool {
    origin: Origin,
    options: PoolOptions,
    tls: Option<tokio_rustls::TlsConnector>,
    idle: Mutex<VecDeque<Idle>>,
    permits: Option<Arc<Semaphore>>,
    raw_heads: bool,
}

impl fmt::Debug for UpstreamPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamPool")
            .field("origin", &self.origin)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl UpstreamPool {
    pub(crate) fn new(
        origin: Origin,
        options: PoolOptions,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Self {
        let tls = if origin.https {
            tls.map(tokio_rustls::TlsConnector::from)
        } else {
            None
        };
        let permits = options
            .connections
            .map(|n| Arc::new(Semaphore::new(n.clamp(1, Semaphore::MAX_PERMITS))));
        Self {
            origin,
            options,
            tls,
            idle: Mutex::new(VecDeque::new()),
            permits,
            raw_heads: false,
        }
    }

    /// This pool, with connections that write the head carried in a request's
    /// [`RAW_HEAD_HEADER`] instead of hyper's (see [`RawHeadIo`]).
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub(crate) fn with_raw_heads(mut self) -> Self {
        self.raw_heads = true;
        self
    }

    pub(crate) fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Sends one request and returns the response once its head has arrived. The response
    /// body keeps the connection (and its place under the connection cap) until it ends; a
    /// connection whose response ended is put back for reuse, one whose body was dropped
    /// early is closed (FWD-21).
    pub(crate) async fn send(
        self: &Arc<Self>,
        request: Request<UpstreamRequestBody>,
    ) -> Result<Response<PooledBody>, PoolError> {
        let permit = match &self.permits {
            Some(permits) => Some(
                Arc::clone(permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| PoolError::ConnectTimeout)?,
            ),
            None => None,
        };
        let mut sender = match self.checkout() {
            Some(sender) => sender,
            None => self.connect().await?,
        };
        let sent = sender.send_request(request);
        let response = match self.options.headers_timeout {
            Some(limit) => tokio::time::timeout(limit, sent)
                .await
                .map_err(|_| PoolError::HeadersTimeout)??,
            None => sent.await?,
        };
        let (parts, body) = response.into_parts();
        let body = PooledBody::new(body, sender, permit, Arc::clone(self));
        Ok(Response::from_parts(parts, body))
    }

    fn checkout(&self) -> Option<SendRequest<UpstreamRequestBody>> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(entry) = idle.pop_back() {
            if entry.since.elapsed() < self.options.keep_alive_timeout
                && !entry.sender.is_closed()
                && entry.sender.is_ready()
            {
                return Some(entry.sender);
            }
        }
        None
    }

    fn checkin(&self, sender: SendRequest<UpstreamRequestBody>) {
        if sender.is_closed() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.retain(|entry| entry.since.elapsed() < self.options.keep_alive_timeout);
        idle.push_back(Idle {
            sender,
            since: Instant::now(),
        });
    }

    async fn connect(&self) -> Result<SendRequest<UpstreamRequestBody>, PoolError> {
        let address = (self.origin.host.as_str(), self.origin.port);
        let tcp = match self.options.connect_timeout {
            Some(limit) => tokio::time::timeout(limit, TcpStream::connect(address))
                .await
                .map_err(|_| PoolError::ConnectTimeout)?,
            None => TcpStream::connect(address).await,
        }
        .map_err(PoolError::Connect)?;
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            Some(connector) => {
                let name = rustls::pki_types::ServerName::try_from(self.origin.host.clone())
                    .map_err(|e| PoolError::Tls(e.to_string()))?;
                let stream = connector
                    .connect(name, tcp)
                    .await
                    .map_err(|e| PoolError::Tls(e.to_string()))?;
                if self.raw_heads {
                    handshake(RawHeadIo::new(stream)).await
                } else {
                    handshake(stream).await
                }
            }
            None if self.origin.https => Err(PoolError::Tls(
                "no TLS configuration for an https upstream".to_string(),
            )),
            None if self.raw_heads => handshake(RawHeadIo::new(tcp)).await,
            None => handshake(tcp).await,
        }
    }
}

async fn handshake<S>(stream: S) -> Result<SendRequest<UpstreamRequestBody>, PoolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) = http1::Builder::new()
        .handshake(TokioIo::new(stream))
        .await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "upstream connection ended with an error");
        }
    });
    Ok(sender)
}

/// The header that carries a request's exact head (hex-encoded) on a connection of a pool
/// made [`with_raw_heads`](UpstreamPool::with_raw_heads).
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub(crate) const RAW_HEAD_HEADER: &str = "x-rust-gateway-raw-head";

/// A connection that writes each request head as the request's [`RAW_HEAD_HEADER`] gives it.
///
/// hyper writes the headers of a map grouped by name, in lower case, after the request line
/// of a parsed URI. The MCP executor must write undici's head instead: names in their case,
/// repeated names apart, a target `http::Uri` refuses (MCP-17, MCP-18). The request hyper
/// sends carries that head and the same framing; this wrapper replaces hyper's head with it
/// and passes the body through, counting it by the head's `content-length` to know where
/// the next head starts. A head without the header, or with a `transfer-encoding`, is
/// written as it is and the connection passes everything through from then on.
struct RawHeadIo<S> {
    inner: S,
    /// The head hyper is writing, until its blank line.
    head: Vec<u8>,
    /// Bytes to write to `inner` before anything else, from `pending_at` on.
    pending: Vec<u8>,
    pending_at: usize,
    /// Body bytes still to pass through before the next head.
    body_left: u64,
    passthrough: bool,
}

impl<S> RawHeadIo<S> {
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    fn new(inner: S) -> Self {
        Self {
            inner,
            head: Vec::new(),
            pending: Vec::new(),
            pending_at: 0,
            body_left: 0,
            passthrough: false,
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> RawHeadIo<S> {
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        while self.pending_at < self.pending.len() {
            let written = std::task::ready!(
                Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_at..])
            )?;
            if written == 0 {
                return Poll::Ready(Err(std::io::ErrorKind::WriteZero.into()));
            }
            self.pending_at += written;
        }
        self.pending.clear();
        self.pending_at = 0;
        Poll::Ready(Ok(()))
    }
}

/// The head to write in place of `head` (a complete head, blank line included), and the
/// length of the body that follows it: `None` when it is not known (`transfer-encoding`).
pub(crate) fn rewrite_head(head: &[u8]) -> (Vec<u8>, Option<u64>) {
    let marker = format!("\r\n{RAW_HEAD_HEADER}:");
    let lower = head.to_ascii_lowercase();
    let replacement = lower
        .windows(marker.len())
        .position(|w| w == marker.as_bytes())
        .and_then(|at| {
            let start = at + marker.len();
            let end = start + head[start..].windows(2).position(|w| w == b"\r\n")?;
            decode_hex(head[start..end].trim_ascii())
        })
        .unwrap_or_else(|| head.to_vec());
    let mut length = Some(0);
    for line in replacement.split(|&b| b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            length = None;
            break;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            length = std::str::from_utf8(value).ok().and_then(|v| v.parse().ok());
        }
    }
    (replacement, length)
}

fn decode_hex(text: &[u8]) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    text.chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for RawHeadIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for RawHeadIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        std::task::ready!(this.poll_pending(cx))?;
        if this.passthrough {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        }
        if this.body_left > 0 {
            let n = buf
                .len()
                .min(usize::try_from(this.body_left).unwrap_or(usize::MAX));
            let written = std::task::ready!(Pin::new(&mut this.inner).poll_write(cx, &buf[..n]))?;
            this.body_left -= written as u64;
            return Poll::Ready(Ok(written));
        }
        let start = this.head.len();
        this.head.extend_from_slice(buf);
        let from = start.saturating_sub(3);
        let Some(end) = this.head[from..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| from + at + 4)
        else {
            return Poll::Ready(Ok(buf.len()));
        };
        this.head.truncate(end);
        let head = std::mem::take(&mut this.head);
        let (replacement, length) = rewrite_head(&head);
        this.pending = replacement;
        this.pending_at = 0;
        match length {
            Some(length) => this.body_left = length,
            None => this.passthrough = true,
        }
        if let Poll::Ready(Err(error)) = this.poll_pending(cx) {
            return Poll::Ready(Err(error));
        }
        Poll::Ready(Ok(end - start))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        std::task::ready!(self.poll_pending(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        std::task::ready!(self.poll_pending(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A response body that holds its connection until it ends.
pub(crate) struct PooledBody {
    inner: Incoming,
    sender: Option<SendRequest<UpstreamRequestBody>>,
    permit: Option<OwnedSemaphorePermit>,
    pool: Arc<UpstreamPool>,
    finished: bool,
    idle_timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl PooledBody {
    fn new(
        inner: Incoming,
        sender: SendRequest<UpstreamRequestBody>,
        permit: Option<OwnedSemaphorePermit>,
        pool: Arc<UpstreamPool>,
    ) -> Self {
        let finished = inner.is_end_stream();
        let idle_timer = pool
            .options
            .body_timeout
            .map(|limit| Box::pin(tokio::time::sleep(limit)));
        Self {
            inner,
            sender: Some(sender),
            permit,
            pool,
            finished,
            idle_timer,
        }
    }
}

impl HttpBody for PooledBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if this.finished {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(timer), Some(limit)) =
                    (this.idle_timer.as_mut(), this.pool.options.body_timeout)
                {
                    timer.as_mut().reset(tokio::time::Instant::now() + limit);
                }
                // A server writing a sized body stops polling after its last byte.
                if this.inner.is_end_stream() {
                    this.finished = true;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.sender = None;
                Poll::Ready(Some(Err(error.into())))
            }
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if let Some(timer) = this.idle_timer.as_mut()
                    && timer.as_mut().poll(cx).is_ready()
                {
                    this.sender = None;
                    return Poll::Ready(Some(Err("the upstream body timed out".into())));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.finished
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for PooledBody {
    fn drop(&mut self) {
        let permit = self.permit.take();
        let Some(mut sender) = self.sender.take() else {
            return;
        };
        if !self.finished {
            // Dropping the sender and the unfinished body closes the connection.
            return;
        }
        let pool = Arc::clone(&self.pool);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            // The connection becomes ready again once hyper has read the whole response.
            if std::future::poll_fn(|cx| sender.poll_ready(cx))
                .await
                .is_ok()
            {
                pool.checkin(sender);
            }
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_heads_replace_hyper_heads() {
        let raw =
            "POST /a/{b}/ HTTP/1.1\r\nhost: h\r\nX-A: 1\r\nx-a: 2\r\ncontent-length: 3\r\n\r\n";
        let hex: String = raw.bytes().map(|b| format!("{b:02x}")).collect();
        let hyper = format!(
            "POST / HTTP/1.1\r\nhost: h\r\nx-a: 1\r\nX-Rust-Gateway-Raw-Head: {hex}\r\ncontent-length: 3\r\n\r\n"
        );
        assert_eq!(
            rewrite_head(hyper.as_bytes()),
            (raw.as_bytes().to_vec(), Some(3))
        );
        let plain = b"GET / HTTP/1.1\r\nhost: h\r\n\r\n";
        assert_eq!(rewrite_head(plain), (plain.to_vec(), Some(0)));
        let chunked = b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n";
        assert_eq!(rewrite_head(chunked).1, None);
    }

    #[tokio::test]
    async fn raw_head_io_rewrites_each_head_and_passes_bodies() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (client, mut server) = tokio::io::duplex(4096);
        let mut io = RawHeadIo::new(client);
        let head = |raw: &str| {
            let hex: String = raw.bytes().map(|b| format!("{b:02x}")).collect();
            format!("GET / HTTP/1.1\r\n{RAW_HEAD_HEADER}: {hex}\r\n\r\n")
        };
        let first = "PUT /{x} HTTP/1.1\r\nA: 1\r\ncontent-length: 4\r\n\r\n";
        let second = "GET /y HTTP/1.1\r\nB: 2\r\n\r\n";
        // Head and body in one write, then a head split over two writes.
        let mut bytes = head(first).into_bytes();
        bytes.extend_from_slice(b"body");
        let mut at = 0;
        while at < bytes.len() {
            at += io.write(&bytes[at..]).await.unwrap();
        }
        let second_head = head(second);
        let (a, b) = second_head.split_at(10);
        io.write_all(a.as_bytes()).await.unwrap();
        io.write_all(b.as_bytes()).await.unwrap();
        io.flush().await.unwrap();
        drop(io);
        let mut out = String::new();
        server.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, format!("{first}body{second}"));
    }

    #[test]
    fn origins_are_parsed_like_undici() {
        let origin = Origin::parse("http://127.0.0.1:2009").unwrap();
        assert_eq!(
            (origin.https, origin.host.as_str(), origin.port),
            (false, "127.0.0.1", 2009)
        );
        assert_eq!(origin.authority(), "127.0.0.1:2009");
        let origin = Origin::parse("https://api.example").unwrap();
        assert_eq!(
            (origin.https, origin.host.as_str(), origin.port),
            (true, "api.example", 443)
        );
        let origin = Origin::parse("http://[::1]:8080").unwrap();
        assert_eq!((origin.host.as_str(), origin.port), ("::1", 8080));
        assert!(Origin::parse("ftp://x").is_err());
        assert!(Origin::parse("not a url").is_err());
    }
}
