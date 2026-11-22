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
    /// A request sent over a pool [`with_raw_heads`](UpstreamPool::with_raw_heads) without
    /// its [`RawHead`]: nothing is sent.
    #[error("the request has no head to write")]
    MissingHead,
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
            Self::MissingHead => "Error",
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

/// One connection: hyper's sender, and for a connection that writes [`RawHead`]s, the slot
/// its [`RawHeadIo`] takes the next head from.
struct Connection {
    sender: SendRequest<UpstreamRequestBody>,
    head_slot: Option<HeadSlot>,
}

struct Idle {
    connection: Connection,
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

    /// This pool, with connections that write the [`RawHead`] each request carries in its
    /// extensions instead of hyper's head (see [`RawHeadIo`]). A request without one is
    /// refused.
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
        mut request: Request<UpstreamRequestBody>,
    ) -> Result<Response<PooledBody>, PoolError> {
        // Only the request's own extension is ever written as a head; nothing a request
        // carries in its headers is.
        let head = request.extensions_mut().remove::<RawHead>();
        if self.raw_heads && head.is_none() {
            return Err(PoolError::MissingHead);
        }
        let permit = match &self.permits {
            Some(permits) => Some(
                Arc::clone(permits)
                    .acquire_owned()
                    .await
                    .map_err(|_| PoolError::ConnectTimeout)?,
            ),
            None => None,
        };
        let mut connection = match self.checkout() {
            Some(connection) => connection,
            None => self.connect().await?,
        };
        if let (Some(slot), Some(head)) = (&connection.head_slot, head) {
            // The connection is checked out, so hyper writes no other head before this one.
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(head);
        }
        let sent = connection.sender.send_request(request);
        let response = match self.options.headers_timeout {
            Some(limit) => tokio::time::timeout(limit, sent)
                .await
                .map_err(|_| PoolError::HeadersTimeout)??,
            None => sent.await?,
        };
        let (parts, body) = response.into_parts();
        let body = PooledBody::new(body, connection, permit, Arc::clone(self));
        Ok(Response::from_parts(parts, body))
    }

    fn checkout(&self) -> Option<Connection> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(entry) = idle.pop_back() {
            if entry.since.elapsed() < self.options.keep_alive_timeout
                && !entry.connection.sender.is_closed()
                && entry.connection.sender.is_ready()
            {
                return Some(entry.connection);
            }
        }
        None
    }

    fn checkin(&self, connection: Connection) {
        if connection.sender.is_closed() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        idle.retain(|entry| entry.since.elapsed() < self.options.keep_alive_timeout);
        idle.push_back(Idle {
            connection,
            since: Instant::now(),
        });
    }

    async fn connect(&self) -> Result<Connection, PoolError> {
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
                    let slot = HeadSlot::default();
                    with_slot(
                        handshake(RawHeadIo::new(stream, Arc::clone(&slot))).await,
                        slot,
                    )
                } else {
                    with_slot(handshake(stream).await, None)
                }
            }
            None if self.origin.https => Err(PoolError::Tls(
                "no TLS configuration for an https upstream".to_string(),
            )),
            None if self.raw_heads => {
                let slot = HeadSlot::default();
                with_slot(
                    handshake(RawHeadIo::new(tcp, Arc::clone(&slot))).await,
                    slot,
                )
            }
            None => with_slot(handshake(tcp).await, None),
        }
    }
}

fn with_slot(
    sender: Result<SendRequest<UpstreamRequestBody>, PoolError>,
    head_slot: impl Into<Option<HeadSlot>>,
) -> Result<Connection, PoolError> {
    Ok(Connection {
        sender: sender?,
        head_slot: head_slot.into(),
    })
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

/// The exact head of a request (request line, headers and blank line) for a connection of a
/// pool made [`with_raw_heads`](UpstreamPool::with_raw_heads), carried as a request
/// extension, and the length of the body that follows it.
///
/// It travels apart from the request's headers on purpose: no header a client, a tool
/// argument or middleware supplies can be read as a head.
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "mcp"), allow(dead_code))]
pub(crate) struct RawHead {
    head: Bytes,
    body_length: u64,
}

impl RawHead {
    /// `head` must be a complete head written by the gateway, and `body_length` the length
    /// of the body the request sends; the request hyper sends must frame its body the same
    /// way (a `content-length` of `body_length`, none when it is 0, never chunked).
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub(crate) fn new(head: impl Into<Bytes>, body_length: u64) -> Self {
        Self {
            head: head.into(),
            body_length,
        }
    }

    #[cfg(all(test, feature = "mcp"))]
    pub(crate) fn head(&self) -> &[u8] {
        &self.head
    }
}

/// The [`RawHead`] of the request a connection is about to write.
type HeadSlot = Arc<Mutex<Option<RawHead>>>;

/// A connection that writes each request's [`RawHead`] in place of hyper's head.
///
/// hyper writes the headers of a map grouped by name, in lower case, after the request line
/// of a parsed URI. The MCP executor must write undici's head instead: names in their case,
/// repeated names apart, a target `http::Uri` refuses (MCP-17, MCP-18). [`UpstreamPool::send`]
/// puts the request's head in the connection's slot before hyper writes it; this wrapper
/// discards hyper's head, writes the slot's instead, and passes the body through, counting it
/// by the head's body length to know where the next head starts. A head hyper writes with no
/// head in the slot, or with a body framed otherwise, fails the request and nothing of it is
/// written.
struct RawHeadIo<S> {
    inner: S,
    slot: HeadSlot,
    /// The head hyper is writing, until its blank line.
    head: Vec<u8>,
    /// Bytes to write to `inner` before anything else, from `pending_at` on.
    pending: Bytes,
    pending_at: usize,
    /// Body bytes still to pass through before the next head.
    body_left: u64,
}

impl<S> RawHeadIo<S> {
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    fn new(inner: S, slot: HeadSlot) -> Self {
        Self {
            inner,
            slot,
            head: Vec::new(),
            pending: Bytes::new(),
            pending_at: 0,
            body_left: 0,
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
        self.pending = Bytes::new();
        self.pending_at = 0;
        Poll::Ready(Ok(()))
    }
}

/// The length of the body that follows a head hyper wrote: its `content-length`, 0 without
/// one, and `None` when it cannot be known from the head (chunked, or several lengths).
fn hyper_body_length(head: &[u8]) -> Option<u64> {
    let mut length = None;
    for line in head.split(|&b| b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let (name, value) = (&line[..colon], line[colon + 1..].trim_ascii());
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return None;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            if length.is_some() {
                return None;
            }
            length = Some(std::str::from_utf8(value).ok()?.parse().ok()?);
        }
    }
    Some(length.unwrap_or(0))
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
        let hyper_head = std::mem::take(&mut this.head);
        let raw = this
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(raw) = raw else {
            return Poll::Ready(Err(std::io::Error::other(
                "no head to write for this request",
            )));
        };
        if hyper_body_length(&hyper_head) != Some(raw.body_length) {
            return Poll::Ready(Err(std::io::Error::other(
                "the request body is not framed as its head says",
            )));
        }
        this.pending = raw.head;
        this.pending_at = 0;
        this.body_left = raw.body_length;
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
    connection: Option<Connection>,
    permit: Option<OwnedSemaphorePermit>,
    pool: Arc<UpstreamPool>,
    finished: bool,
    idle_timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl PooledBody {
    fn new(
        inner: Incoming,
        connection: Connection,
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
            connection: Some(connection),
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
                this.connection = None;
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
                    this.connection = None;
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
        let Some(mut connection) = self.connection.take() else {
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
            if std::future::poll_fn(|cx| connection.sender.poll_ready(cx))
                .await
                .is_ok()
            {
                pool.checkin(connection);
            }
            drop(permit);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hyper_body_lengths() {
        assert_eq!(
            hyper_body_length(b"GET / HTTP/1.1\r\nhost: h\r\n\r\n"),
            Some(0)
        );
        assert_eq!(
            hyper_body_length(b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\n"),
            Some(3)
        );
        assert_eq!(
            hyper_body_length(b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"),
            None
        );
        assert_eq!(
            hyper_body_length(b"POST / HTTP/1.1\r\ncontent-length: 3\r\ncontent-length: 3\r\n\r\n"),
            None
        );
    }

    async fn written(
        io: RawHeadIo<tokio::io::DuplexStream>,
        mut server: tokio::io::DuplexStream,
        writes: &[&[u8]],
    ) -> (std::io::Result<()>, String) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut io = io;
        let mut result = Ok(());
        for bytes in writes {
            if let Err(error) = io.write_all(bytes).await {
                result = Err(error);
                break;
            }
        }
        if result.is_ok() {
            result = io.flush().await;
        }
        drop(io);
        let mut out = String::new();
        server.read_to_string(&mut out).await.unwrap();
        (result, out)
    }

    #[tokio::test]
    async fn raw_head_io_writes_the_slot_head_and_passes_bodies() {
        let slot = HeadSlot::default();
        let (client, server) = tokio::io::duplex(4096);
        let io = RawHeadIo::new(client, Arc::clone(&slot));
        let first = "PUT /{x} HTTP/1.1\r\nA: 1\r\ncontent-length: 4\r\n\r\n";
        *slot.lock().unwrap() = Some(RawHead::new(first, 4));
        // hyper's head, whatever it holds, is never written; its body is.
        let hyper_first =
            b"PUT / HTTP/1.1\r\nx-rust-gateway-raw-head: 474554\r\ncontent-length: 4\r\n\r\nbody";
        let mut io = io;
        {
            use tokio::io::AsyncWriteExt as _;
            io.write_all(hyper_first).await.unwrap();
        }
        let second = "GET /y HTTP/1.1\r\nB: 2\r\n\r\n";
        *slot.lock().unwrap() = Some(RawHead::new(second, 0));
        let hyper_second = b"GET / HTTP/1.1\r\nb: 2\r\n\r\n";
        let (a, b) = hyper_second.split_at(10);
        let (result, out) = written(io, server, &[a, b]).await;
        result.unwrap();
        assert_eq!(out, format!("{first}body{second}"));
    }

    #[tokio::test]
    async fn raw_head_io_refuses_a_head_it_was_not_given() {
        // No head in the slot: nothing is written.
        let (client, server) = tokio::io::duplex(4096);
        let io = RawHeadIo::new(client, HeadSlot::default());
        let (result, out) = written(io, server, &[b"GET / HTTP/1.1\r\nhost: h\r\n\r\n"]).await;
        assert!(result.is_err());
        assert_eq!(out, "");
        // hyper framing its body otherwise than the head says: nothing is written.
        for (hyper, length) in [
            (
                &b"POST / HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n"[..],
                0,
            ),
            (&b"POST / HTTP/1.1\r\ncontent-length: 3\r\n\r\n"[..], 4),
            (&b"POST / HTTP/1.1\r\n\r\n"[..], 4),
        ] {
            let slot = HeadSlot::default();
            *slot.lock().unwrap() = Some(RawHead::new("POST /x HTTP/1.1\r\n\r\n", length));
            let (client, server) = tokio::io::duplex(4096);
            let (result, out) = written(RawHeadIo::new(client, slot), server, &[hyper]).await;
            assert!(result.is_err());
            assert_eq!(out, "");
        }
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
