//! The HTTP server: what the Reference Gateway gets from Node's HTTP server and from Express,
//! reproduced around the axum router.
//!
//! - [`bind_dual_stack`]: listen on `::` with IPv4-mapped addresses, as Node does, so an
//!   IPv4 client is seen as `::ffff:127.0.0.1` (FWD-07, RL-06).
//! - [`serve`]: an HTTP/1.1 accept loop that gives handlers the client address and answers
//!   `Expect: 100-continue` with `100 Continue` at once, as Node does (FWD-12).
//! - [`node_compat`]: Express's router checks that run before any route (malformed
//!   percent-encoding, REQ-26; the asterisk-form target, REQ-01), and the headers Express and
//!   Node put on every response, in their order (FWD-15, FWD-22, REQ-23): `X-Powered-By`
//!   first, then the response's own headers, `Content-Length` for a body of known length that
//!   is not streamed, `Date`, `Connection` and `Keep-Alive`, and `Transfer-Encoding: chunked`
//!   when there is no length.
//! - Request targets as Node's parser accepts them ([`inbound`]).
//! - WebSocket upgrades (area 06), when the gateway serves itself
//!   ([`Gateway::serve`](crate::Gateway::serve)): an upgrade request never reaches hyper or
//!   the router (WS-01). Its head is held back on the way in; hyper finishes whatever it is
//!   doing on the connection and is shut down gracefully; the raw connection is taken back
//!   from it ([`hyper::server::conn::http1::Connection::into_parts`]) and handed to the
//!   WebSocket proxy, which writes every byte the client receives itself. That is how a
//!   connection can be closed with no response (WS-06, WS-16) or answered with a bare
//!   status line (WS-15), which no handler behind hyper can do.

use std::future::Future;
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::middleware::Next;
use http::header::{self, HeaderName, HeaderValue};
use http::{Extensions, HeaderMap, Method, Request, Response, StatusCode, Version};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tower::ServiceExt as _;

use crate::error::GatewayError;

mod inbound;
mod upgrade;
use crate::gateway::Shared;
use crate::request::{X_POWERED_BY, malformed_path_segment};
pub(crate) use inbound::rewrite_target;
pub(crate) use upgrade::{Takeover, UpgradeHead};

/// Binds a listener on every address, IPv6 and IPv4 alike, like Node's `listen(port)`:
/// `[::]:port` with IPv4-mapped addresses enabled, and Node's backlog of 511. Falls back to
/// `0.0.0.0:port` where IPv6 is unavailable.
pub fn bind_dual_stack(port: u16) -> io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let bind = |domain: Domain, address: SocketAddr| -> io::Result<std::net::TcpListener> {
        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
        if domain == Domain::IPV6 {
            socket.set_only_v6(false)?;
        }
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&address.into())?;
        socket.listen(511)?;
        Ok(socket.into())
    };
    let listener = bind(
        Domain::IPV6,
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
    )
    .or_else(|_| bind(Domain::IPV4, SocketAddr::from(([0, 0, 0, 0], port))))?;
    TcpListener::from_std(listener)
}

/// Serves `router` on `listener` until `shutdown` resolves (CFG-18: the router may hold the
/// application's routes). Then it stops accepting, lets open connections finish their
/// current request, and returns.
pub async fn serve(
    router: Router,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    serve_with(router, None, listener, shutdown).await
}

/// [`serve`], with WebSocket upgrades proxied by `gateway` (see the [module docs](self)).
/// On shutdown, open tunnels are closed.
pub(crate) async fn serve_gateway(
    router: Router,
    gateway: Arc<Shared>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    serve_with(router, Some(gateway), listener, shutdown).await
}

async fn serve_with(
    router: Router,
    gateway: Option<Arc<Shared>>,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut connections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                // Usually too many open files; keep serving.
                tracing::warn!(%error, "accepting a connection failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        match &gateway {
            Some(gateway) => connections.spawn(serve_gateway_connection(
                router.clone(),
                Arc::clone(gateway),
                stream,
                peer,
                stop_rx.clone(),
            )),
            None => connections.spawn(serve_connection(
                router.clone(),
                stream,
                peer,
                stop_rx.clone(),
            )),
        };
        while connections.try_join_next().is_some() {}
    }
    drop(listener);
    let _ = stop_tx.send(true);
    while connections.join_next().await.is_some() {}
    Ok(())
}

async fn serve_connection(
    router: Router,
    stream: TcpStream,
    peer: SocketAddr,
    mut stop: watch::Receiver<bool>,
) {
    let interim = Interim::default();
    let io = ConnIo {
        inner: stream,
        interim: interim.clone(),
        inbound: inbound::Inbound::default(),
        ready: bytes::BytesMut::new(),
        eof: false,
        takeover: None,
    };
    let service = router_service(router, peer, interim);
    let connection = hyper::server::conn::http1::Builder::new()
        .auto_date_header(false)
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::pin!(connection);
    loop {
        tokio::select! {
            result = connection.as_mut() => {
                if let Err(error) = result {
                    tracing::debug!(%error, "client connection ended with an error");
                }
                return;
            }
            changed = stop.changed() => {
                connection.as_mut().graceful_shutdown();
                if changed.is_err() {
                    // The server is gone: still let the connection finish.
                    let _ = connection.as_mut().await;
                    return;
                }
            }
        }
    }
}

type BoxedResponse =
    Pin<Box<dyn Future<Output = Result<Response<Body>, std::convert::Infallible>> + Send>>;

/// The hyper service for one connection: the router, with the client address and the
/// connection's [`Interim`] in each request's extensions.
fn router_service(
    router: Router,
    peer: SocketAddr,
    interim: Interim,
) -> impl hyper::service::Service<
    Request<Incoming>,
    Response = Response<Body>,
    Error = std::convert::Infallible,
    Future = BoxedResponse,
> + Unpin {
    hyper::service::service_fn(move |mut request: Request<Incoming>| -> BoxedResponse {
        request.extensions_mut().insert(ConnectInfo(peer));
        request.extensions_mut().insert(interim.clone());
        let router = router.clone();
        Box::pin(async move { router.oneshot(request.map(Body::new)).await })
    })
}

/// A connection of a gateway that proxies WebSocket upgrades itself: served by hyper until
/// an upgrade request arrives, then taken over (see the [module docs](self)).
async fn serve_gateway_connection(
    router: Router,
    gateway: Arc<Shared>,
    stream: TcpStream,
    peer: SocketAddr,
    mut stop: watch::Receiver<bool>,
) {
    let interim = Interim::default();
    let takeover = Arc::new(Notify::new());
    let io = ConnIo {
        inner: stream,
        interim: interim.clone(),
        inbound: inbound::Inbound::holding_upgrades(),
        ready: bytes::BytesMut::new(),
        eof: false,
        takeover: Some(Arc::clone(&takeover)),
    };
    let mut connection = hyper::server::conn::http1::Builder::new()
        .auto_date_header(false)
        .serve_connection(TokioIo::new(io), router_service(router, peer, interim));
    let mut stopping = false;
    let result = loop {
        tokio::select! {
            result = std::future::poll_fn(|cx| connection.poll_without_shutdown(cx)) => {
                break result;
            }
            () = takeover.notified() => Pin::new(&mut connection).graceful_shutdown(),
            _ = stop.changed(), if !stopping => {
                // The server is stopping (or gone): finish the current request, take
                // nothing over.
                stopping = true;
                Pin::new(&mut connection).graceful_shutdown();
            }
        }
    };
    let parts = connection.into_parts();
    let mut io = parts.io.into_inner();
    let held = match result {
        Ok(()) if !stopping => io.inbound.take_held(),
        Ok(()) => None,
        Err(error) => {
            tracing::debug!(%error, "client connection ended with an error");
            None
        }
    };
    let Some(held) = held else {
        // What hyper would have done at the end of the connection.
        let _ = io.inner.shutdown().await;
        return;
    };
    if !parts.read_buf.is_empty() || !io.ready.is_empty() {
        // hyper consumes whole requests only, and nothing after an upgrade head reaches it.
        tracing::debug!("bytes left before an upgrade request; closing the connection");
        return;
    }
    let takeover = Takeover {
        head: held.head,
        stream: io.inner,
        early: held.early.freeze(),
    };
    crate::proxy::websocket::handle(&gateway, takeover, peer, stop).await;
}

/// Bytes to write on a connection before the next response: Node's immediate
/// `100 Continue` (FWD-12).
#[derive(Clone, Default)]
pub(crate) struct Interim {
    pending: Arc<Mutex<Vec<u8>>>,
}

impl Interim {
    fn push(&self, bytes: &[u8]) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
    }
}

/// The client connection: request heads rewritten on the way in ([`inbound`]), [`Interim`]
/// bytes written ahead of hyper's on the way out.
struct ConnIo {
    inner: TcpStream,
    interim: Interim,
    inbound: inbound::Inbound,
    /// Rewritten bytes not yet handed to hyper.
    ready: bytes::BytesMut,
    eof: bool,
    /// Signalled when an upgrade head is held back and the connection must be taken over.
    takeover: Option<Arc<Notify>>,
}

impl ConnIo {
    fn poll_interim(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut pending = self
            .interim
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !pending.is_empty() {
            match Pin::new(&mut self.inner).poll_write(cx, &pending) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                Poll::Ready(Ok(n)) => {
                    pending.drain(..n);
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for ConnIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if !this.ready.is_empty() {
                let n = this.ready.len().min(buf.remaining());
                buf.put_slice(&this.ready.split_to(n));
                return Poll::Ready(Ok(()));
            }
            if this.eof {
                return Poll::Ready(Ok(()));
            }
            if this.inbound.is_holding() {
                // Nothing more for hyper: the server shuts it down and takes over. No waker
                // is kept; the server polls the connection again after shutting it down.
                if let Some(takeover) = &this.takeover {
                    takeover.notify_one();
                }
                return Poll::Pending;
            }
            let mut received = [0u8; 16 * 1024];
            let mut received = ReadBuf::new(&mut received);
            std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, &mut received))?;
            if received.filled().is_empty() {
                this.inbound.finish(&mut this.ready);
                this.eof = true;
            } else {
                this.inbound.feed(received.filled(), &mut this.ready);
            }
        }
    }
}

impl AsyncWrite for ConnIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        std::task::ready!(self.poll_interim(cx))?;
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        std::task::ready!(self.poll_interim(cx))?;
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(self.poll_interim(cx))?;
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(self.poll_interim(cx))?;
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// How a response's framing headers are completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseKind {
    /// Written with Express's `res.send`/`res.json`: the length is known and sent.
    Express,
    /// Written as a stream (`res.write`/`pipe`): no `Content-Length` is added, so a body
    /// without one is sent chunked.
    Streamed,
    /// A proxied upstream response (FWD-15): streamed.
    Proxied,
}

/// Records how a response must be completed.
pub(crate) fn mark(extensions: &mut Extensions, kind: ResponseKind) {
    extensions.insert(kind);
}

/// What [`node_compat`] needs to know about the request to complete its response.
struct RequestInfo {
    method: Method,
    keep_alive: bool,
}

impl RequestInfo {
    fn of(request: &Request<Body>) -> Self {
        let connection: Vec<String> = request
            .headers()
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(|token| token.trim().to_ascii_lowercase())
            .collect();
        let keep_alive = match request.version() {
            Version::HTTP_10 | Version::HTTP_09 => connection.iter().any(|t| t == "keep-alive"),
            _ => !connection.iter().any(|t| t == "close"),
        };
        Self {
            method: request.method().clone(),
            keep_alive,
        }
    }
}

/// The Express and Node behaviours around every request (see the [module docs](self)).
pub(crate) async fn node_compat(
    State(shared): State<Arc<Shared>>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    let info = RequestInfo::of(&request);
    if request.version() != Version::HTTP_10
        && request
            .headers()
            .get(header::EXPECT)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
        && let Some(interim) = request.extensions().get::<Interim>()
    {
        interim.push(b"HTTP/1.1 100 Continue\r\n\r\n");
    }

    let target = request.uri().to_string();
    let early = if target == "*" {
        Some(GatewayError::NotFound {
            method: request.method().to_string(),
            path: target,
        })
    } else {
        malformed_path_segment(&target).map(|segment| GatewayError::MalformedPath { segment })
    };
    let response = match early {
        Some(error) => crate::proxy::render_error(&shared, &error),
        None => next.run(request).await,
    };
    complete(response, &info, shared.x_powered_by)
}

/// Completes the response headers the way Node writes them.
fn complete(response: Response<Body>, info: &RequestInfo, x_powered_by: bool) -> Response<Body> {
    let (mut parts, body) = response.into_parts();
    let kind = parts
        .extensions
        .get::<ResponseKind>()
        .copied()
        .unwrap_or(ResponseKind::Express);
    let status = parts.status;
    let has_body = info.method != Method::HEAD
        && !status.is_informational()
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED;
    if status == StatusCode::SWITCHING_PROTOCOLS {
        return Response::from_parts(parts, body);
    }

    let old = std::mem::take(&mut parts.headers);
    let mut headers = HeaderMap::with_capacity(old.len() + 6);
    let powered_by = HeaderName::from_static("x-powered-by");
    if x_powered_by && !old.contains_key(&powered_by) {
        headers.append(powered_by, HeaderValue::from_static(X_POWERED_BY));
    }
    for (name, value) in &old {
        headers.append(name.clone(), value.clone());
    }
    let has_length = headers.contains_key(header::CONTENT_LENGTH);
    let has_encoding = headers.contains_key(header::TRANSFER_ENCODING);
    if !has_length
        && !has_encoding
        && kind == ResponseKind::Express
        && status != StatusCode::NO_CONTENT
        && status != StatusCode::NOT_MODIFIED
        && let Some(length) = body.size_hint().exact()
    {
        headers.append(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    if !headers.contains_key(header::DATE)
        && let Ok(date) = HeaderValue::from_str(&httpdate::fmt_http_date(SystemTime::now()))
    {
        headers.append(header::DATE, date);
    }
    if info.keep_alive {
        headers.append(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.append(
            HeaderName::from_static("keep-alive"),
            HeaderValue::from_static("timeout=5"),
        );
    } else {
        headers.append(header::CONNECTION, HeaderValue::from_static("close"));
    }
    let unsized_body = !headers.contains_key(header::CONTENT_LENGTH);
    if has_body && unsized_body && !has_encoding {
        headers.append(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
    }
    parts.headers = headers;
    // Node writes its own reason phrase, whatever the upstream sent (FWD-14).
    parts
        .extensions
        .insert(hyper::ext::ReasonPhrase::from_static(
            node_reason(status.as_u16()).as_bytes(),
        ));
    if unsized_body {
        // axum adds a `Content-Length` for a body of known size after this layer has run;
        // Node sends such a response without one.
        return Response::from_parts(parts, Body::new(Unsized(body)));
    }
    Response::from_parts(parts, body)
}

/// A body that does not tell its size, so that no `Content-Length` is derived from it.
struct Unsized(Body);

impl http_body::Body for Unsized {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, axum::Error>>> {
        Pin::new(&mut self.0).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }
}

/// Node's reason phrase for a status code (`http.STATUS_CODES`, Node v24.20.0), or
/// `unknown` for a code it does not register (FWD-14).
pub(crate) fn node_reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        102 => "Processing",
        103 => "Early Hints",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        204 => "No Content",
        205 => "Reset Content",
        206 => "Partial Content",
        207 => "Multi-Status",
        208 => "Already Reported",
        226 => "IM Used",
        300 => "Multiple Choices",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        305 => "Use Proxy",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        416 => "Range Not Satisfiable",
        417 => "Expectation Failed",
        418 => "I'm a Teapot",
        421 => "Misdirected Request",
        422 => "Unprocessable Entity",
        423 => "Locked",
        424 => "Failed Dependency",
        425 => "Too Early",
        426 => "Upgrade Required",
        428 => "Precondition Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        451 => "Unavailable For Legal Reasons",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        506 => "Variant Also Negotiates",
        507 => "Insufficient Storage",
        508 => "Loop Detected",
        509 => "Bandwidth Limit Exceeded",
        510 => "Not Extended",
        511 => "Network Authentication Required",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_phrases_are_nodes() {
        assert_eq!(node_reason(200), "OK");
        assert_eq!(node_reason(418), "I'm a Teapot");
        assert_eq!(node_reason(299), "unknown");
        assert_eq!(node_reason(599), "unknown");
    }
}
