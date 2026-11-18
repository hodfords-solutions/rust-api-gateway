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
        }
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
                handshake(stream).await
            }
            None if self.origin.https => Err(PoolError::Tls(
                "no TLS configuration for an https upstream".to_string(),
            )),
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
