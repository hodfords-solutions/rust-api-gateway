//! WebSocket proxying (area 06): the reference's `handleWebSocketRequest`,
//! `WsRequestService`, `forwardWebsocket` and `ProxySocket`.
//!
//! The server hands over an upgrade request with its raw connection ([`Takeover`]); every
//! byte the client receives from then on is written here.
//!
//! An upgrade never goes through the Route lookup or the proxy middleware, so it is gated
//! on its own (Deviation D15, as in the Reference Gateway from 12.5.0):
//!
//! 1. Sanitise (WS-02): normalise the URL, remove the `excludeHeaders`.
//! 2. Drop the connection unless it is a `GET` asking for `websocket` (WS-06).
//! 3. Refuse with 403 unless the URL is under a `websocket_route_prefixes` entry (WS-17).
//! 4. Select the Upstream Service (WS-03).
//! 5. Count the upgrade against the Global IP Bucket (WS-18).
//! 6. Run the WebSocket middleware (WS-04); a rejection counts against the Anonymous IP
//!    Bucket (WS-18). Every refusal of steps 3 to 6 is answered with the error status and
//!    JSON body, and the connection closed (Deviation D1, WS-05).
//! 7. Open a new plain-HTTP connection to the service host and send the upgrade request with
//!    the full URL and every client header, middleware headers merged in (WS-07 to WS-09).
//!    If that fails, close the client connection (WS-16).
//! 8. On `101`, answer `HTTP/1.1 101 Switching Protocols` with the upstream's headers
//!    (WS-10) and tunnel bytes both ways, the bytes that came with either handshake first
//!    (WS-11, WS-12), with no idle timeout (WS-13), closing each side when the other closes
//!    (WS-14). Otherwise answer 502 and close, never passing the upstream's answer on (WS-15).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Response, Uri};
use http_body_util::BodyExt as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::error::GatewayError;
use crate::gateway::Shared;
use crate::handler::{GatewayRequest, WsProxyMiddleware};
use crate::request::{
    JsObject, NodeHeaderValue, ProxyRequest, is_valid_header_value, latin1, matches_prefix_segment,
    node_incoming_headers, normalize_request_url,
};
use crate::server::{Takeover, UpgradeHead, node_reason, rewrite_target};

use super::{Failure, rate_limit_error, render_error_with};

/// The longest upstream response head read, like Node's default `maxHeaderSize`; a longer
/// one fails the upgrade (WS-16).
const MAX_RESPONSE_HEAD: usize = 16 * 1024;

/// Handles one upgrade request taken over by the server, until its tunnel closes or the
/// server stops.
pub(crate) async fn handle(
    shared: &Arc<Shared>,
    takeover: Takeover,
    peer: SocketAddr,
    stop: watch::Receiver<bool>,
) {
    let Takeover {
        head,
        stream,
        early,
    } = takeover;
    let _ = stream.set_nodelay(true);
    // `socket.setKeepAlive(true, 0)`: TCP keep-alive probes, not an idle timeout (WS-13).
    let _ = socket2::SockRef::from(&stream).set_keepalive(true);
    tokio::select! {
        () = proxy(shared, head, stream, early, peer) => {}
        () = stopped(stop) => {}
    }
}

async fn stopped(mut stop: watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn proxy(
    shared: &Arc<Shared>,
    head: UpgradeHead,
    mut client: TcpStream,
    early: Bytes,
    peer: SocketAddr,
) {
    // OTEL-03, OTEL-04: the upstream handshake's client span, a child of the client's trace
    // context (an upgrade has no server span), started once the request is about to be
    // written; its `traceparent` (and `tracestate`) are set like the reference's HTTP
    // instrumentation sets them in `http.request`'s headers object.
    #[cfg(feature = "otel")]
    let mut trace: Option<crate::otel::ClientSpan> = None;
    #[cfg(feature = "otel")]
    let inject = |upstream: &UpstreamAddress, url: &str, user_agent: Option<&str>| {
        trace = crate::otel::ClientSpan::start_from_headers(
            head.headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_bytes())),
            head.method.as_str(),
            &format!("http://{}:{}{url}", upstream.hostname, upstream.port),
            user_agent,
        );
        trace
            .as_ref()
            .map(|span| span.trace_headers())
            .unwrap_or_default()
    };
    #[cfg(not(feature = "otel"))]
    let inject = |_: &UpstreamAddress, _: &str, _: Option<&str>| Vec::new();
    let outgoing = match prepare(shared, &head, peer, inject).await {
        Ok(outgoing) => outgoing,
        Err(Refusal::Reject(failure)) => {
            #[cfg(feature = "otel")]
            if let Some(span) = trace.take() {
                span.fail(&failure.error, "ERR_INVALID_CHAR");
            }
            if let GatewayError::Internal {
                source: Some(source),
            } = &failure.error
            {
                tracing::error!(error = %source, "WebSocket upgrade failed");
            }
            answer_and_close(shared, client, &failure).await;
            return;
        }
        // `socket.destroy()`.
        Err(Refusal::Drop) => return,
    };
    let mut upstream = match connect(&outgoing).await {
        Ok(upstream) => upstream,
        Err(error) => {
            #[cfg(feature = "otel")]
            if let Some(span) = &trace {
                span.fail(&error, crate::otel::io_error_type(&error));
            }
            tracing::debug!(%error, "WebSocket upstream unreachable");
            // `socket.end()`: no response (WS-16).
            let _ = client.shutdown().await;
            return;
        }
    };
    let mut buffer = BytesMut::new();
    let response = match read_response_head(&mut upstream, &mut buffer).await {
        Ok(response) => response,
        Err(error) => {
            #[cfg(feature = "otel")]
            if let Some(span) = &trace {
                span.fail(&error, crate::otel::io_error_type(&error));
            }
            tracing::debug!(%error, "WebSocket upstream failed before its response");
            let _ = client.shutdown().await;
            return;
        }
    };
    // The client span covers the handshake, not the tunnel.
    #[cfg(feature = "otel")]
    if let Some(span) = trace.take() {
        span.response(response.status);
    }
    if response.status == 101 {
        let first = switching_protocols(&response, &buffer);
        if let Err(error) = splice(client, upstream, &first, &early).await {
            tracing::debug!(%error, "WebSocket tunnel closed with an error");
        }
    } else {
        // WS-15 (Deviation D15): the upgrade skipped the Route lookup, the proxy middleware
        // and the Route rate limits, so relaying the upstream's answer would let any client
        // read the upstream's HTTP API by adding `Upgrade: websocket`.
        drop(upstream);
        answer_and_close(shared, client, &GatewayError::UpgradeNotSwitched.into()).await;
    }
}

/// Why an upgrade is not forwarded.
enum Refusal {
    /// Answered with this error, then closed (Deviation D1, WS-05).
    Reject(Failure),
    /// Closed with no response (WS-06).
    Drop,
}

impl From<GatewayError> for Refusal {
    fn from(error: GatewayError) -> Self {
        Refusal::Reject(error.into())
    }
}

impl From<Failure> for Refusal {
    fn from(failure: Failure) -> Self {
        Refusal::Reject(failure)
    }
}

/// Where and what the upgrade is forwarded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outgoing {
    upstream: UpstreamAddress,
    /// The upstream request head, exactly as written.
    request: Vec<u8>,
}

/// Steps 1 to 4 (see the [module docs](self)).
/// `inject` gives the trace headers of the upstream request (OTEL-04), from the upstream's
/// address, the request target and the client's `user-agent`.
async fn prepare(
    shared: &Arc<Shared>,
    head: &UpgradeHead,
    peer: SocketAddr,
    inject: impl FnOnce(&UpstreamAddress, &str, Option<&str>) -> Vec<(&'static str, String)>,
) -> Result<Outgoing, Refusal> {
    // WS-02.
    let url = normalize_request_url(&head.target).map_err(GatewayError::internal)?;
    let exclude = &shared.options.exclude_headers;
    let lines = strip_excluded(&head.headers, exclude);

    // WS-06 (Deviation D15: before anything else, so nothing is counted or run for it).
    let incoming = node_incoming_headers(lines.iter().map(|(n, v)| (n, v)));
    let upgrade = incoming.iter().find_map(|(name, value)| match value {
        NodeHeaderValue::Single(value) if name == "upgrade" => Some(value.as_slice()),
        _ => None,
    });
    if !is_websocket_upgrade(&head.method, upgrade) {
        return Err(Refusal::Drop);
    }

    // WS-17 (Deviation D15).
    if !is_websocket_route(&url, &shared.options.websocket_route_prefixes) {
        return Err(GatewayError::Forbidden.into());
    }

    // WS-03.
    let store = &shared.store;
    let prefix = shared
        .selector
        .select(&url, |prefix| store.has_document(prefix))
        .into_result()?
        .prefix
        .to_string();

    let mut headers = HeaderMap::with_capacity(lines.len());
    for (name, value) in &lines {
        headers.append(name.clone(), value.clone());
    }

    // WS-18 (Deviation D15): the Global IP Bucket, before any middleware, as for a Routed
    // Request (RL-09).
    let limiter = &shared.limiter;
    let mut state = limiter.request_state(Some(peer.ip()), &headers);
    limiter
        .count_global_ip(&mut state)
        .await
        .map_err(|error| rate_limit_error(error, &state))?;

    // WS-04.
    let uri = Uri::try_from(head.target.as_str())
        .or_else(|_| Uri::try_from(rewrite_target(head.target.as_bytes()).as_slice()))
        .unwrap_or_else(|_| Uri::from_static("/"));
    let mut request =
        GatewayRequest::new(head.method.clone(), uri, url.clone(), headers, Some(peer))
            .with_version(head.version);
    let mut proxy_request = ProxyRequest::new();
    if let Err(rejection) = run_middleware(
        shared.handlers.websocket.iter(),
        exclude,
        &mut request,
        &mut proxy_request,
    )
    .await
    {
        // WS-18 (Deviation D15): a rejected upgrade counts against the Anonymous IP Bucket
        // first, as a rejected Routed Request does (RL-10).
        limiter
            .count_anonymous_ip(&mut state)
            .await
            .map_err(|error| rate_limit_error(error, &state))?;
        return Err(rejection.into());
    }

    // WS-07 to WS-09.
    let host = shared
        .options
        .api_services
        .iter()
        .find(|service| service.prefix == prefix)
        .map(|service| service.host.as_str())
        .ok_or(GatewayError::NoUpstreamService)?;
    let Some(upstream) = UpstreamAddress::from_host(host) else {
        // `new URL(host)` throws; the build already rejects such hosts.
        return Err(Refusal::Drop);
    };
    let user_agent = incoming.iter().find_map(|(name, value)| match value {
        NodeHeaderValue::Single(value) if name == "user-agent" => std::str::from_utf8(value).ok(),
        _ => None,
    });
    let trace = inject(&upstream, &url, user_agent);
    let request = upstream_request_head(
        &head.method,
        &url,
        incoming,
        &proxy_request.kebab_headers(),
        &trace,
        &upstream,
    )?;
    Ok(Outgoing { upstream, request })
}

/// The client's header lines without the `excludeHeaders`, matched case-insensitively
/// (WS-02).
fn strip_excluded(
    lines: &[(HeaderName, HeaderValue)],
    exclude: &[String],
) -> Vec<(HeaderName, HeaderValue)> {
    lines
        .iter()
        .filter(|(name, _)| {
            !exclude
                .iter()
                .any(|excluded| name.as_str().eq_ignore_ascii_case(excluded))
        })
        .cloned()
        .collect()
}

/// `WsRequestService.handle` (WS-04): removes the excluded names from the outgoing header
/// set, then runs the middleware in order (the registry keeps them in ascending priority);
/// the first that does not allow the upgrade stops the chain with 403, or with its own
/// error.
async fn run_middleware<'a>(
    middleware: impl IntoIterator<Item = &'a Arc<dyn WsProxyMiddleware>>,
    exclude: &[String],
    request: &mut GatewayRequest,
    proxy_request: &mut ProxyRequest,
) -> Result<(), GatewayError> {
    for name in exclude {
        proxy_request.remove_header(name);
    }
    for middleware in middleware {
        match middleware.handle(request, proxy_request).await {
            Ok(true) => {}
            Ok(false) => return Err(GatewayError::Forbidden),
            Err(error) => return Err(error.into_gateway_error()),
        }
    }
    Ok(())
}

/// `ProxySocket.checkMethodAndHeader` (WS-06): a `GET` whose `Upgrade` is `websocket` in
/// any letter case.
fn is_websocket_upgrade(method: &Method, upgrade: Option<&[u8]>) -> bool {
    method == Method::GET && upgrade.is_some_and(|value| value.eq_ignore_ascii_case(b"websocket"))
}

/// Whether upgrades are accepted on the normalised `url` (WS-17, Deviation D15): it is under
/// a `websocket_route_prefixes` entry, on whole path segments. No entry, no route.
fn is_websocket_route<S: AsRef<str>>(url: &str, prefixes: &[S]) -> bool {
    prefixes
        .iter()
        .any(|prefix| matches_prefix_segment(url, prefix.as_ref()))
}

/// Where an upgrade is sent (WS-09): the service host's name and port, over plain HTTP
/// whatever its scheme. As with Node's `http.request({ hostname, port: url.port })`, a port
/// the URL leaves out, or that is the scheme's default (`https://h:443`), means 80.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UpstreamAddress {
    /// The host name or address to connect to (an IPv6 address without brackets).
    connect_host: String,
    /// `url.hostname` (an IPv6 address in brackets), for a `Host` header.
    hostname: String,
    port: u16,
}

impl UpstreamAddress {
    fn from_host(host: &str) -> Option<Self> {
        let url = url::Url::parse(host).ok()?;
        let (connect_host, hostname) = match url.host()? {
            url::Host::Domain(domain) => (domain.to_string(), domain.to_string()),
            url::Host::Ipv4(ip) => (ip.to_string(), ip.to_string()),
            url::Host::Ipv6(ip) => (ip.to_string(), format!("[{ip}]")),
        };
        Some(Self {
            connect_host,
            hostname,
            port: url.port().unwrap_or(80),
        })
    }
}

/// The upstream request head (WS-07, WS-08) as Node's `http.request` writes it for the
/// reference's options: `GET <url> HTTP/1.1`, then `{...req.headers, ...extraHeaders}`: the
/// client's headers as Node joined them (names lower-cased, in JavaScript key order),
/// middleware headers replacing same-named ones in place or appended, `set-cookie` once per
/// value. Node adds `Host` when there is none, and `Connection: close` when there is no
/// `connection` header; nothing else (no `x-forwarded-for`, no length for a `GET`).
///
/// Fails with [`GatewayError::UpstreamUnavailable`] when Node would throw on a middleware
/// header: a name that is not a token, or a value with a character other than tab,
/// printable ASCII and U+0080 to U+00FF.
///
/// `trace` (empty unless tracing is started, OTEL-04) is set last, as the reference's HTTP
/// instrumentation sets it on the headers object: replacing a same-named header in place,
/// appended otherwise.
fn upstream_request_head(
    method: &Method,
    url: &str,
    incoming: Vec<(String, NodeHeaderValue)>,
    extra: &[(String, String)],
    trace: &[(&'static str, String)],
    upstream: &UpstreamAddress,
) -> Result<Vec<u8>, GatewayError> {
    let mut merged: JsObject<NodeHeaderValue> = JsObject::new();
    for (name, value) in incoming {
        merged.insert(name, value);
    }
    for (name, value) in extra {
        let bytes = latin1(value)
            .filter(|bytes| is_valid_header_value(bytes))
            .ok_or(GatewayError::UpstreamUnavailable)?;
        if HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(GatewayError::UpstreamUnavailable);
        }
        merged.insert(name.clone(), NodeHeaderValue::Single(bytes));
    }
    for (name, value) in trace {
        merged.insert(
            (*name).to_string(),
            NodeHeaderValue::Single(value.clone().into_bytes()),
        );
    }
    let entries = merged.into_entries();
    let has = |wanted: &str| entries.iter().any(|(n, _)| n.eq_ignore_ascii_case(wanted));
    let (has_host, has_connection) = (has("host"), has("connection"));

    let mut out = format!("{method} {url} HTTP/1.1\r\n").into_bytes();
    for (name, value) in &entries {
        let values: Vec<&[u8]> = match value {
            NodeHeaderValue::Single(value) => vec![value.as_slice()],
            NodeHeaderValue::Multiple(values) => values.iter().map(Vec::as_slice).collect(),
        };
        for value in values {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value);
            out.extend_from_slice(b"\r\n");
        }
    }
    if !has_host {
        let host = match upstream.port {
            80 => upstream.hostname.clone(),
            port => format!("{}:{port}", upstream.hostname),
        };
        out.extend_from_slice(format!("Host: {host}\r\n").as_bytes());
    }
    if !has_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

/// Opens a new connection (never a pooled one, WS-09) and sends the upgrade request.
async fn connect(outgoing: &Outgoing) -> std::io::Result<TcpStream> {
    let address = &outgoing.upstream;
    let mut stream = TcpStream::connect((address.connect_host.as_str(), address.port)).await?;
    let _ = stream.set_nodelay(true);
    stream.write_all(&outgoing.request).await?;
    Ok(stream)
}

/// An upstream response head.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResponseHead {
    status: u16,
    /// Node's `res.headers`.
    headers: Vec<(String, NodeHeaderValue)>,
}

/// Reads the upstream's response head, skipping informational responses other than `101`
/// as Node does. `buffer` keeps the bytes read after the head.
async fn read_response_head(
    upstream: &mut TcpStream,
    buffer: &mut BytesMut,
) -> std::io::Result<ResponseHead> {
    loop {
        let end = loop {
            if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            if buffer.len() > MAX_RESPONSE_HEAD {
                return Err(invalid("upstream response head too large"));
            }
            if upstream.read_buf(buffer).await? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        };
        let raw = buffer.split_to(end);
        let head =
            parse_response_head(&raw).ok_or_else(|| invalid("bad upstream response head"))?;
        if (100..200).contains(&head.status) && head.status != 101 {
            continue;
        }
        return Ok(head);
    }
}

fn invalid(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Parses a complete response head as Node's HTTP client accepts it.
fn parse_response_head(raw: &[u8]) -> Option<ResponseHead> {
    let mut storage = vec![httparse::EMPTY_HEADER; 2000];
    let mut response = httparse::Response::new(&mut storage);
    if !matches!(response.parse(raw), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    let status = response.code?;
    let mut lines = Vec::with_capacity(response.headers.len());
    let mut has_content_length = false;
    let mut chunked = false;
    for line in response.headers.iter() {
        let name = HeaderName::from_bytes(line.name.as_bytes()).ok()?;
        let value = HeaderValue::from_bytes(line.value).ok()?;
        if name == header::CONTENT_LENGTH {
            let digits = value.as_bytes().trim_ascii();
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            has_content_length = true;
        } else if name == header::TRANSFER_ENCODING {
            chunked = value
                .as_bytes()
                .rsplit(|&b| b == b',')
                .next()
                .is_some_and(|last| last.trim_ascii().eq_ignore_ascii_case(b"chunked"));
        }
        lines.push((name, value));
    }
    if has_content_length && chunked {
        // llhttp rejects a response with both.
        return None;
    }
    let headers = node_incoming_headers(lines.iter().map(|(n, v)| (n, v)));
    Some(ResponseHead { status, headers })
}

/// `ProxySocket.createHttpHeader`: the status line, one `name: value` line per value, and
/// the empty line. Node holds header values as latin1 strings and writes the head as UTF-8,
/// so a byte above `0x7f` is written as its two-byte UTF-8 form.
fn http_head(line: &str, headers: &[(String, NodeHeaderValue)]) -> Vec<u8> {
    let mut out = line.as_bytes().to_vec();
    for (name, value) in headers {
        let values: Vec<&[u8]> = match value {
            NodeHeaderValue::Single(value) => vec![value.as_slice()],
            NodeHeaderValue::Multiple(values) => values.iter().map(Vec::as_slice).collect(),
        };
        for value in values {
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(latin1_as_utf8(value).as_bytes());
        }
    }
    out.extend_from_slice(b"\r\n\r\n");
    out
}

fn latin1_as_utf8(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// What the client gets first when the upstream upgrades (WS-10, WS-12): `HTTP/1.1 101
/// Switching Protocols` with the upstream's headers, then the bytes the upstream sent after
/// its own head.
fn switching_protocols(response: &ResponseHead, upstream_head: &[u8]) -> Vec<u8> {
    let mut first = http_head("HTTP/1.1 101 Switching Protocols", &response.headers);
    first.extend_from_slice(upstream_head);
    first
}

/// Starts the tunnel: `first` to the client, the bytes the client sent after its request
/// head to the upstream (WS-12), then bytes both ways (WS-11) with no idle timeout (WS-13).
/// When one side ends its stream, the other side's write half is shut down; when either
/// side fails, both connections are dropped, which closes them (WS-14).
async fn splice<C, U>(
    mut client: C,
    mut upstream: U,
    first: &[u8],
    early: &[u8],
) -> std::io::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    client.write_all(first).await?;
    if !early.is_empty() {
        upstream.write_all(early).await?;
    }
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let to_upstream = async {
        tokio::io::copy(&mut client_read, &mut upstream_write).await?;
        upstream_write.shutdown().await
    };
    let to_client = async {
        tokio::io::copy(&mut upstream_read, &mut client_write).await?;
        client_write.shutdown().await
    };
    tokio::try_join!(to_upstream, to_client).map(|_| ())
}

/// Deviation D1 (WS-05): the rejection is answered with the error the HTTP path would give
/// (status, headers and JSON body of the error renderer) and the connection is closed.
async fn answer_and_close(shared: &Arc<Shared>, mut client: TcpStream, failure: &Failure) {
    let rendered = render_error_with(shared, &failure.error, &failure.context);
    let bytes = closing_response(rendered, shared.x_powered_by).await;
    let _ = client.write_all(&bytes).await;
    let _ = client.shutdown().await;
}

/// A complete response with `Connection: close`, as bytes.
async fn closing_response(response: Response<axum::body::Body>, x_powered_by: bool) -> Vec<u8> {
    let (parts, body) = response.into_parts();
    let body = body
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .unwrap_or_default();
    let status = parts.status.as_u16();
    let mut out = format!("HTTP/1.1 {status} {}\r\n", node_reason(status));
    if x_powered_by && !parts.headers.contains_key("x-powered-by") {
        out.push_str(&format!(
            "X-Powered-By: {}\r\n",
            crate::request::X_POWERED_BY
        ));
    }
    let mut out = out.into_bytes();
    for (name, value) in &parts.headers {
        if [
            header::CONTENT_LENGTH,
            header::TRANSFER_ENCODING,
            header::CONNECTION,
            header::DATE,
        ]
        .contains(name)
        {
            continue;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    let date = httpdate::fmt_http_date(std::time::SystemTime::now());
    out.extend_from_slice(
        format!(
            "Content-Length: {}\r\nDate: {date}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(body.chunk());
    out
}

#[cfg(test)]
mod tests;
