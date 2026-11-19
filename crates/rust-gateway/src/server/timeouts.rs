//! Client connection timeouts (REQ-28, REQ-29): Node's `headersTimeout`, `requestTimeout`
//! and `keepAliveTimeout`, which bound how long a client may hold a connection without
//! completing a request.
//!
//! Node (v24) tracks every connection's current request from its first byte: the
//! connection is "active" from the start of a message until the message is complete (a
//! fresh connection is active from the moment it is accepted). An active connection whose
//! head is not complete after `headersTimeout`, or whose request is not complete after
//! `requestTimeout`, gets `HTTP/1.1 408 Request Timeout` with `Connection: close` (unless a
//! response is being written on it) and is closed. Once a response has finished, the socket
//! gets an inactivity timeout of `keepAliveTimeout` plus `keepAliveTimeoutBuffer` until the
//! next request's head is complete; when it passes, the connection is closed without a word.
//!
//! Node checks the first two every `connectionsCheckingInterval` (30 s), so it expires a
//! request somewhere between the timeout and 30 s later; this gateway expires it at the
//! timeout. The 408 is written on hyper's next read of the connection, which comes at once
//! while a head or a body is awaited; if the handler is not reading the body at that moment
//! (it is still waiting for something else), the connection is closed without it.
//!
//! The [`Clock`] follows one connection: the server reports reads and writes, the request
//! head reaching the router, the end of the request body, and the start and end of the
//! response; the connection's task sleeps until [`Clock::deadline`] and asks
//! [`Clock::expire`] what to do.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// What Node writes before closing a connection whose request timed out.
pub(crate) const REQUEST_TIMEOUT_RESPONSE: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\nConnection: close\r\n\r\n";

/// The client connection timeouts of the gateway's HTTP server (REQ-28, REQ-29): the
/// options of Node's `http.Server` that the Reference Gateway runs with. The defaults are
/// Node's; [`Duration::ZERO`] turns a timeout off, as `0` does in Node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerTimeouts {
    /// `headersTimeout`: how long a request head may take to arrive, from its first byte
    /// (from the connection's start for its first request). Default 60 s.
    pub headers_timeout: Duration,
    /// `requestTimeout`: how long a whole request, head and body, may take to arrive.
    /// Default 300 s.
    pub request_timeout: Duration,
    /// `keepAliveTimeout`: how long a connection may stay idle after a response, announced
    /// in the `Keep-Alive: timeout=<seconds>` response header. Default 5 s; zero also drops
    /// the `Keep-Alive` header.
    pub keep_alive_timeout: Duration,
    /// `keepAliveTimeoutBuffer`: added to `keepAliveTimeout` for the connection's actual
    /// inactivity timeout, so that a client reusing the connection at the announced limit
    /// does not race the close. Default 1 s.
    pub keep_alive_timeout_buffer: Duration,
}

impl Default for ServerTimeouts {
    fn default() -> Self {
        Self {
            headers_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(300),
            keep_alive_timeout: Duration::from_secs(5),
            keep_alive_timeout_buffer: Duration::from_secs(1),
        }
    }
}

impl ServerTimeouts {
    /// The `Keep-Alive` response header value Node writes on a keep-alive response, if any:
    /// `timeout=` and the whole seconds of `keepAliveTimeout`.
    pub(crate) fn keep_alive_header(&self) -> Option<String> {
        (!self.keep_alive_timeout.is_zero())
            .then(|| format!("timeout={}", self.keep_alive_timeout.as_secs()))
    }
}

/// What a passed deadline means for the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Expiry {
    /// The request took too long: answer [`REQUEST_TIMEOUT_RESPONSE`] when `respond`, then
    /// close.
    Request { respond: bool },
    /// The connection stayed idle after a response: close it.
    Idle,
}

/// The timing state of one client connection (see the [module docs](self)).
#[derive(Debug)]
pub(crate) struct Clock {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Debug)]
struct State {
    timeouts: ServerTimeouts,
    /// When the current request started; `None` between requests.
    message_start: Option<Instant>,
    /// Whether the current request's head is complete.
    headers_done: bool,
    /// The last socket activity, while the keep-alive timeout is armed.
    idle_since: Option<Instant>,
    /// A response is being written.
    responding: bool,
    /// The request timed out: the next read answers 408 (or not) and fails.
    expired: Option<bool>,
    /// The request timed out and the connection is closing: nothing more is written.
    closed: bool,
    /// The connection was handed over (an upgrade): no more timeouts.
    stopped: bool,
}

impl Clock {
    /// The clock of a connection accepted now, which Node counts as the start of its first
    /// request.
    pub(crate) fn new(timeouts: ServerTimeouts) -> Self {
        Self {
            state: Mutex::new(State {
                timeouts,
                message_start: Some(Instant::now()),
                headers_done: false,
                idle_since: None,
                responding: false,
                expired: None,
                closed: false,
                stopped: false,
            }),
            changed: Notify::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Uses `timeouts` from now on (the application's, learnt from its first response when
    /// the server was not given them).
    pub(crate) fn set_timeouts(&self, timeouts: ServerTimeouts) {
        let mut state = self.state();
        if state.timeouts != timeouts {
            state.timeouts = timeouts;
            drop(state);
            self.changed.notify_one();
        }
    }

    /// Bytes arrived from the client: the start of a request when none is in progress, and
    /// activity for the keep-alive timeout.
    pub(crate) fn on_read(&self) {
        let now = Instant::now();
        let mut state = self.state();
        if state.idle_since.is_some() {
            state.idle_since = Some(now);
        }
        if state.message_start.is_none() {
            state.message_start = Some(now);
            state.headers_done = false;
        }
    }

    /// Bytes were written to the client: activity for the keep-alive timeout.
    pub(crate) fn on_write(&self) {
        let mut state = self.state();
        if state.idle_since.is_some() {
            state.idle_since = Some(Instant::now());
        }
    }

    /// A request head is complete (it reached the router).
    pub(crate) fn on_headers(&self) {
        let mut state = self.state();
        state.message_start.get_or_insert_with(Instant::now);
        state.headers_done = true;
        state.idle_since = None;
    }

    /// The request body has been read to its end (or given up by the handler).
    pub(crate) fn on_request_complete(&self) {
        let mut state = self.state();
        state.message_start = None;
        state.headers_done = false;
    }

    /// A response is about to be written.
    pub(crate) fn on_response(&self) {
        self.state().responding = true;
    }

    /// The response has been written: the keep-alive timeout starts.
    pub(crate) fn on_response_end(&self) {
        let mut state = self.state();
        state.responding = false;
        if !state.timeouts.keep_alive_timeout.is_zero() {
            state.idle_since = Some(Instant::now());
        }
        drop(state);
        self.changed.notify_one();
    }

    /// The connection was handed over (a WebSocket upgrade): it never times out.
    pub(crate) fn stop(&self) {
        self.state().stopped = true;
        self.changed.notify_one();
    }

    /// Resolves when the deadline may have moved earlier.
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    /// The earliest moment a timeout can expire, if any is running.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let state = self.state();
        if state.stopped {
            return None;
        }
        let t = &state.timeouts;
        let mut deadlines = Vec::with_capacity(3);
        if let Some(start) = state.message_start {
            if !state.headers_done && !t.headers_timeout.is_zero() {
                deadlines.push(start + t.headers_timeout);
            }
            if !t.request_timeout.is_zero() {
                deadlines.push(start + t.request_timeout);
            }
        }
        if let Some(since) = state.idle_since {
            deadlines.push(since + t.keep_alive_timeout + t.keep_alive_timeout_buffer);
        }
        deadlines.into_iter().min()
    }

    /// What has expired at `now`, if anything. A request that expired is remembered for
    /// [`take_expired`](Self::take_expired).
    pub(crate) fn expire(&self, now: Instant) -> Option<Expiry> {
        let mut state = self.state();
        if state.stopped {
            return None;
        }
        let t = state.timeouts;
        if let Some(start) = state.message_start {
            let headers = !state.headers_done
                && !t.headers_timeout.is_zero()
                && now >= start + t.headers_timeout;
            let request = !t.request_timeout.is_zero() && now >= start + t.request_timeout;
            if headers || request {
                let respond = !state.responding;
                state.expired = Some(respond);
                return Some(Expiry::Request { respond });
            }
        }
        match state.idle_since {
            Some(since) if now >= since + t.keep_alive_timeout + t.keep_alive_timeout_buffer => {
                Some(Expiry::Idle)
            }
            _ => None,
        }
    }

    /// Whether the request timed out, and if so whether to answer 408 before closing. Taken
    /// once.
    pub(crate) fn take_expired(&self) -> Option<bool> {
        let mut state = self.state();
        let expired = state.expired.take();
        state.closed |= expired.is_some();
        expired
    }

    /// Whether the connection is closing after a timeout, so that no response written after
    /// the 408 reaches the client.
    pub(crate) fn is_closed(&self) -> bool {
        self.state().closed
    }
}

/// A request body that tells the [`Clock`] when it has been read to its end, or given up.
pub(crate) struct RequestBody {
    body: hyper::body::Incoming,
    clock: Option<Arc<Clock>>,
}

impl RequestBody {
    pub(crate) fn new(body: hyper::body::Incoming, clock: Arc<Clock>) -> Self {
        let mut tracked = Self {
            body,
            clock: Some(clock),
        };
        if http_body::Body::is_end_stream(&tracked.body) {
            tracked.complete();
        }
        tracked
    }

    fn complete(&mut self) {
        if let Some(clock) = self.clock.take() {
            clock.on_request_complete();
        }
    }
}

impl http_body::Body for RequestBody {
    type Data = bytes::Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, hyper::Error>>> {
        let frame = std::task::ready!(Pin::new(&mut self.body).poll_frame(cx));
        if frame.is_none() || self.body.is_end_stream() {
            self.complete();
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Drop for RequestBody {
    fn drop(&mut self) {
        self.complete();
    }
}

/// A response body that tells the [`Clock`] when it has been written to its end, or given
/// up.
pub(crate) struct ResponseBody {
    body: axum::body::Body,
    clock: Option<Arc<Clock>>,
}

impl ResponseBody {
    pub(crate) fn new(body: axum::body::Body, clock: Arc<Clock>) -> Self {
        clock.on_response();
        Self {
            body,
            clock: Some(clock),
        }
    }

    fn end(&mut self) {
        if let Some(clock) = self.clock.take() {
            clock.on_response_end();
        }
    }
}

impl http_body::Body for ResponseBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<bytes::Bytes>, axum::Error>>> {
        let frame = std::task::ready!(Pin::new(&mut self.body).poll_frame(cx));
        if frame.is_none() {
            self.end();
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        self.end();
    }
}

/// Sleeps until `deadline`, or forever without one.
pub(crate) async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeouts() -> ServerTimeouts {
        ServerTimeouts {
            headers_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(300),
            keep_alive_timeout: Duration::from_secs(5),
            keep_alive_timeout_buffer: Duration::from_secs(1),
        }
    }

    // REQ-28: a connection that sends nothing, or only part of a head, expires after
    // `headersTimeout` from the connection's start, with a 408.
    #[tokio::test(start_paused = true)]
    async fn req_28_a_head_expires_after_the_headers_timeout_from_the_connection_start() {
        let clock = Clock::new(timeouts());
        let start = Instant::now();
        assert_eq!(clock.deadline(), Some(start + Duration::from_secs(60)));
        tokio::time::advance(Duration::from_secs(30)).await;
        clock.on_read();
        assert_eq!(clock.expire(Instant::now()), None);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(
            clock.expire(Instant::now()),
            Some(Expiry::Request { respond: true })
        );
        assert!(!clock.is_closed());
        assert_eq!(clock.take_expired(), Some(true));
        assert_eq!(clock.take_expired(), None);
        assert!(clock.is_closed());
    }

    // REQ-28: once the head is complete, only `requestTimeout` bounds the body.
    #[tokio::test(start_paused = true)]
    async fn req_28_a_body_expires_after_the_request_timeout() {
        let clock = Clock::new(timeouts());
        let start = Instant::now();
        clock.on_headers();
        assert_eq!(clock.deadline(), Some(start + Duration::from_secs(300)));
        tokio::time::advance(Duration::from_secs(299)).await;
        assert_eq!(clock.expire(Instant::now()), None);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            clock.expire(Instant::now()),
            Some(Expiry::Request { respond: true })
        );
    }

    // REQ-28: a request that times out while its response is being written is closed
    // without a 408.
    #[tokio::test(start_paused = true)]
    async fn req_28_no_408_while_a_response_is_written() {
        let clock = Clock::new(timeouts());
        clock.on_headers();
        clock.on_response();
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(
            clock.expire(Instant::now()),
            Some(Expiry::Request { respond: false })
        );
    }

    // REQ-29: after a response, the connection may stay idle for `keepAliveTimeout` plus
    // the buffer; activity restarts the count; a complete head stops it, and the next
    // request's own timeouts start with its first byte.
    #[tokio::test(start_paused = true)]
    async fn req_29_keep_alive_timeout_after_a_response() {
        let clock = Clock::new(timeouts());
        clock.on_headers();
        clock.on_request_complete();
        clock.on_response();
        clock.on_response_end();
        assert_eq!(
            clock.deadline(),
            Some(Instant::now() + Duration::from_secs(6))
        );
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(clock.expire(Instant::now()), None);
        // The next request's first bytes: the idle timer restarts and its head timeout runs.
        clock.on_read();
        let read_at = Instant::now();
        assert_eq!(clock.deadline(), Some(read_at + Duration::from_secs(6)));
        tokio::time::advance(Duration::from_secs(6)).await;
        assert_eq!(clock.expire(Instant::now()), Some(Expiry::Idle));
        clock.on_headers();
        assert_eq!(clock.deadline(), Some(read_at + Duration::from_secs(300)));
    }

    // CFG-20: the defaults are Node v24's.
    #[test]
    fn cfg_20_defaults_are_nodes() {
        assert_eq!(ServerTimeouts::default(), timeouts());
    }

    // Zero turns a timeout off; a handed-over connection never times out.
    #[tokio::test(start_paused = true)]
    async fn zero_turns_a_timeout_off_and_an_upgrade_stops_the_clock() {
        let off = ServerTimeouts {
            headers_timeout: Duration::ZERO,
            request_timeout: Duration::ZERO,
            keep_alive_timeout: Duration::ZERO,
            keep_alive_timeout_buffer: Duration::from_secs(1),
        };
        let clock = Clock::new(off);
        assert_eq!(clock.deadline(), None);
        clock.on_headers();
        clock.on_request_complete();
        clock.on_response_end();
        assert_eq!(clock.deadline(), None);
        assert_eq!(off.keep_alive_header(), None);
        assert_eq!(timeouts().keep_alive_header().as_deref(), Some("timeout=5"));

        let clock = Clock::new(timeouts());
        clock.stop();
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(clock.deadline(), None);
        assert_eq!(clock.expire(Instant::now()), None);
    }
}
