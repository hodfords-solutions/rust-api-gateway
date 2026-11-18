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
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tower::ServiceExt as _;

use crate::error::GatewayError;

mod inbound;
use crate::gateway::Shared;
use crate::request::{X_POWERED_BY, malformed_path_segment};

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
        connections.spawn(serve_connection(
            router.clone(),
            stream,
            peer,
            stop_rx.clone(),
        ));
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
    };
    let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
        request.extensions_mut().insert(ConnectInfo(peer));
        request.extensions_mut().insert(interim.clone());
        let router = router.clone();
        async move { router.oneshot(request.map(Body::new)).await }
    });
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
