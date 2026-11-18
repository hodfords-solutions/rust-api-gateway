//! Body adapters for forwarding (FWD-13, FWD-17, FWD-21, FWD-23, RL-16).

use std::future::Future as _;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use http_body::{Body as HttpBody, Frame, SizeHint};
use http_body_util::BodyExt as _;

use crate::handler::BoxError;
use crate::request::BodyLength;

/// How long [`take_arrived`] waits for each further part of a body that is still arriving.
const ARRIVAL_WAIT: Duration = Duration::from_millis(10);

/// The most [`take_arrived`] reads ahead before streaming the rest.
const ARRIVAL_LIMIT: usize = 64 * 1024;

/// The client's request body on its way upstream: what had arrived when the request was
/// dispatched, then the rest as it arrives. Nothing is buffered beyond that first part.
pub(crate) struct UpstreamRequestBody {
    arrived: Option<Bytes>,
    rest: Option<Body>,
    length: Option<u64>,
}

impl UpstreamRequestBody {
    pub(crate) fn empty() -> Self {
        Self {
            arrived: None,
            rest: None,
            length: Some(0),
        }
    }

    /// A body that is all there: `bytes`, then the end.
    #[cfg_attr(not(feature = "mcp"), allow(dead_code))]
    pub(crate) fn full(bytes: Bytes) -> Self {
        let length = Some(bytes.len() as u64);
        Self {
            arrived: (!bytes.is_empty()).then_some(bytes),
            rest: None,
            length,
        }
    }
}

impl HttpBody for UpstreamRequestBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        if let Some(arrived) = self.arrived.take() {
            return Poll::Ready(Some(Ok(Frame::data(arrived))));
        }
        let Some(rest) = self.rest.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(rest).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                Ok(data) => Poll::Ready(Some(Ok(Frame::data(data)))),
                // Trailers of a chunked request are not forwarded (undici reads only data).
                Err(_) => Poll::Ready(None),
            },
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => {
                self.rest = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.arrived.is_none() && self.rest.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        match self.length {
            Some(n) => SizeHint::with_exact(n),
            None => SizeHint::default(),
        }
    }
}

/// Reads what has already arrived of the client's body, the way undici sees a request body
/// stream when the request is dispatched (FWD-10, FWD-13): a body that has fully arrived has a
/// known length; one still arriving is streamed.
///
/// Parts already received by the server are taken at once; each further part is waited for
/// at most [`ARRIVAL_WAIT`], and at most [`ARRIVAL_LIMIT`] bytes are read ahead.
pub(crate) async fn take_arrived(
    mut body: Body,
    declared: Option<u64>,
) -> Result<(BodyLength, UpstreamRequestBody), BoxError> {
    if body.is_end_stream() {
        return Ok((BodyLength::Empty, UpstreamRequestBody::empty()));
    }
    let mut arrived = BytesMut::new();
    loop {
        if arrived.len() > ARRIVAL_LIMIT {
            break;
        }
        match tokio::time::timeout(ARRIVAL_WAIT, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    arrived.extend_from_slice(&data);
                }
            }
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => {
                let length = arrived.len() as u64;
                let body = UpstreamRequestBody {
                    arrived: (!arrived.is_empty()).then(|| arrived.freeze()),
                    rest: None,
                    length: Some(length),
                };
                let known = if length == 0 {
                    BodyLength::Empty
                } else {
                    BodyLength::Known(length)
                };
                return Ok((known, body));
            }
            Err(_still_arriving) => break,
        }
    }
    let body = UpstreamRequestBody {
        arrived: (!arrived.is_empty()).then(|| arrived.freeze()),
        rest: Some(body),
        length: declared,
    };
    Ok((BodyLength::Streaming, body))
}

/// The upstream response body on its way to the client.
///
/// When the upstream fails mid-body (FWD-23), the bytes received so far must reach the client
/// before the server closes the connection. The server stops at the first error without
/// flushing what it buffered in the same pass, so the error is held back briefly: the body
/// reports "not ready" once, letting the server flush, and fails on the next poll.
pub(crate) struct ClientResponseBody<B> {
    inner: B,
    held_error: Option<BoxError>,
    delay: Option<Pin<Box<tokio::time::Sleep>>>,
    on_end: Option<Box<dyn FnOnce() + Send>>,
    ended: bool,
}

impl<B> ClientResponseBody<B>
where
    B: HttpBody<Data = Bytes, Error = BoxError> + Send + Unpin + 'static,
{
    pub(crate) fn new(inner: B) -> Self {
        let ended = inner.is_end_stream();
        Self {
            inner,
            held_error: None,
            delay: None,
            on_end: None,
            ended,
        }
    }

    /// Runs `hook` once the body has been sent completely (the reference's `finish` event),
    /// after the hooks given before it. A body that is empty from the start counts as sent
    /// when the response is dropped. A hook that never runs is dropped with the body.
    pub(crate) fn on_end(mut self, hook: impl FnOnce() + Send + 'static) -> Self {
        self.on_end = Some(match self.on_end.take() {
            Some(first) => Box::new(move || {
                first();
                hook();
            }),
            None => Box::new(hook),
        });
        self
    }

    fn finish(&mut self) {
        self.ended = true;
        if let Some(hook) = self.on_end.take() {
            hook();
        }
    }
}

impl<B> HttpBody for ClientResponseBody<B>
where
    B: HttpBody<Data = Bytes, Error = BoxError> + Send + Unpin + 'static,
{
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        if let Some(delay) = this.delay.as_mut() {
            if delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            this.delay = None;
            let error = this
                .held_error
                .take()
                .unwrap_or_else(|| "the upstream response failed".into());
            this.on_end = None;
            return Poll::Ready(Some(Err(error)));
        }
        if this.ended {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Err(error))) => {
                this.held_error = Some(error);
                let mut delay = Box::pin(tokio::time::sleep(Duration::from_millis(5)));
                let _ = delay.as_mut().poll(cx);
                this.delay = Some(delay);
                Poll::Pending
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Ok(frame))) => {
                // A server writing a sized body stops polling after its last byte.
                if this.inner.is_end_stream() {
                    this.finish();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended && self.delay.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for ClientResponseBody<B> {
    fn drop(&mut self) {
        if self.ended
            && let Some(hook) = self.on_end.take()
        {
            hook();
        }
    }
}
