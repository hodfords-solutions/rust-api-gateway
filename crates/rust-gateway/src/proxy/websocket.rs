//! WebSocket proxying (area 06): the reference's `handleWebSocketRequest`,
//! `WsRequestService`, `forwardWebsocket` and `ProxySocket`.
//!
//! The server hands over an upgrade request with its raw connection ([`Takeover`]); every
//! byte the client receives from then on is written here.
//!
//! 1. Sanitise (WS-02): normalise the URL, remove the `excludeHeaders`.
//! 2. Select the Upstream Service (WS-03).
//! 3. Run the WebSocket middleware (WS-04). A rejection, or no Upstream Service, is answered
//!    with the error status and JSON body, and the connection closed (Deviation D1, WS-05).
//! 4. Drop the connection unless it is a `GET` asking for `websocket` (WS-06).
//! 5. Open a new plain-HTTP connection to the service host and send the upgrade request with
//!    the full URL and every client header, middleware headers merged in (WS-07 to WS-09).
//!    If that fails, close the client connection (WS-16).
//! 6. On `101`, answer `HTTP/1.1 101 Switching Protocols` with the upstream's headers
//!    (WS-10) and tunnel bytes both ways, the bytes that came with either handshake first
//!    (WS-11, WS-12), with no idle timeout (WS-13), closing each side when the other closes
//!    (WS-14). Otherwise pass on the status line and the body, then close (WS-15).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use http::header::{self, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Response, Uri, Version};
use http_body_util::BodyExt as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::error::GatewayError;
use crate::gateway::Shared;
use crate::handler::{GatewayRequest, WsProxyMiddleware};
use crate::request::{
    JsObject, NodeHeaderValue, ProxyRequest, is_valid_header_value, latin1, node_incoming_headers,
    normalize_request_url,
};
use crate::server::{Takeover, UpgradeHead, node_reason, rewrite_target};

use super::render_error;

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
    let outgoing = match prepare(shared, &head, peer).await {
        Ok(outgoing) => outgoing,
        Err(Refusal::Reject(error)) => {
            if let GatewayError::Internal {
                source: Some(source),
            } = &error
            {
                tracing::error!(error = %source, "WebSocket upgrade failed");
            }
            answer_and_close(shared, client, &error).await;
            return;
        }
        // `socket.destroy()`.
        Err(Refusal::Drop) => return,
    };
    let mut upstream = match connect(&outgoing).await {
        Ok(upstream) => upstream,
        Err(error) => {
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
            tracing::debug!(%error, "WebSocket upstream failed before its response");
            let _ = client.shutdown().await;
            return;
        }
    };
    if response.status == 101 {
        let first = switching_protocols(&response, &buffer);
        if let Err(error) = splice(client, upstream, &first, &early).await {
            tracing::debug!(%error, "WebSocket tunnel closed with an error");
        }
    } else {
        refuse(client, upstream, buffer, &response, head.version).await;
    }
}

/// Why an upgrade is not forwarded.
enum Refusal {
    /// Answered with this error, then closed (Deviation D1, WS-05).
    Reject(GatewayError),
    /// Closed with no response (WS-06).
    Drop,
}

impl From<GatewayError> for Refusal {
    fn from(error: GatewayError) -> Self {
        Refusal::Reject(error)
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
async fn prepare(
    shared: &Arc<Shared>,
    head: &UpgradeHead,
    peer: SocketAddr,
) -> Result<Outgoing, Refusal> {
    // WS-02.
    let url = normalize_request_url(&head.target).map_err(GatewayError::internal)?;
    let exclude = &shared.options.exclude_headers;
    let lines = strip_excluded(&head.headers, exclude);

    // WS-03.
    let store = &shared.store;
    let prefix = shared
        .selector
        .select(&url, |prefix| store.has_document(prefix))
        .into_result()?
        .prefix
        .to_string();

    // WS-04.
    let mut headers = HeaderMap::with_capacity(lines.len());
    for (name, value) in &lines {
        headers.append(name.clone(), value.clone());
    }
    let uri = Uri::try_from(head.target.as_str())
        .or_else(|_| Uri::try_from(rewrite_target(head.target.as_bytes()).as_slice()))
        .unwrap_or_else(|_| Uri::from_static("/"));
    let mut request =
        GatewayRequest::new(head.method.clone(), uri, url.clone(), headers, Some(peer))
            .with_version(head.version);
    let mut proxy_request = ProxyRequest::new();
    run_middleware(
        shared.handlers.websocket.iter(),
        exclude,
        &mut request,
        &mut proxy_request,
    )
    .await?;

    // WS-06.
    let incoming = node_incoming_headers(lines.iter().map(|(n, v)| (n, v)));
    let upgrade = incoming.iter().find_map(|(name, value)| match value {
        NodeHeaderValue::Single(value) if name == "upgrade" => Some(value.as_slice()),
        _ => None,
    });
    if !is_websocket_upgrade(&head.method, upgrade) {
        return Err(Refusal::Drop);
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
    let request = upstream_request_head(
        &head.method,
        &url,
        incoming,
        &proxy_request.kebab_headers(),
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
fn upstream_request_head(
    method: &Method,
    url: &str,
    incoming: Vec<(String, NodeHeaderValue)>,
    extra: &[(String, String)],
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
    /// The reason phrase as sent.
    reason: Vec<u8>,
    /// Node's `res.headers`.
    headers: Vec<(String, NodeHeaderValue)>,
    has_content_length: bool,
    chunked: bool,
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
    let line_end = raw.windows(2).position(|w| w == b"\r\n")?;
    // `HTTP/1.1 401 Reason phrase`: everything after the code and its space.
    let reason = raw[..line_end].get(13..).unwrap_or_default().to_vec();
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
    Some(ResponseHead {
        status,
        reason,
        headers,
        has_content_length,
        chunked,
    })
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

/// The status line the client gets when the upstream does not upgrade (WS-15):
/// `HTTP/<client version> <status> <reason>`.
fn refusal_line(version: Version, status: u16, reason: &[u8]) -> String {
    let version = if version == Version::HTTP_10 {
        "1.0"
    } else {
        "1.1"
    };
    format!("HTTP/{version} {status} {}", latin1_as_utf8(reason))
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

/// `ProxySocket.onResponse` (WS-15): the status line, `transfer-encoding: chunked` when the
/// client spoke HTTP/1.1 and the upstream sent no `content-length` (the body then chunk
/// framed), the body, and the end of the connection.
async fn refuse(
    mut client: TcpStream,
    mut upstream: TcpStream,
    buffer: BytesMut,
    response: &ResponseHead,
    client_version: Version,
) {
    let chunked = client_version == Version::HTTP_11 && !response.has_content_length;
    let extra: Vec<(String, NodeHeaderValue)> = if chunked {
        vec![(
            "transfer-encoding".into(),
            NodeHeaderValue::Single(b"chunked".to_vec()),
        )]
    } else {
        Vec::new()
    };
    let line = refusal_line(client_version, response.status, &response.reason);
    if client.write_all(&http_head(&line, &extra)).await.is_err() {
        return;
    }
    let mut body = UpstreamBody::new(response, buffer);
    loop {
        match body.next(&mut upstream).await {
            Ok(Some(data)) => {
                let framed = if chunked {
                    let mut framed = format!("{:x}\r\n", data.len()).into_bytes();
                    framed.extend_from_slice(&data);
                    framed.extend_from_slice(b"\r\n");
                    framed
                } else {
                    data.to_vec()
                };
                if client.write_all(&framed).await.is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(error) => {
                // Node leaves the client connection as it is; there is nothing more to send.
                tracing::debug!(%error, "upstream body failed after a refused upgrade");
                return;
            }
        }
    }
    if chunked && client.write_all(b"0\r\n\r\n").await.is_err() {
        return;
    }
    // `socket.destroySoon()`.
    let _ = client.shutdown().await;
}

/// The body of an upstream response, decoded from its framing.
struct UpstreamBody {
    buffer: BytesMut,
    framing: Framing,
}

enum Framing {
    Length(u64),
    Chunked(ChunkState),
    UntilClose,
    Done,
}

#[derive(Clone, Copy)]
enum ChunkState {
    Size,
    Data(u64),
    DataEnd,
    Trailers,
}

impl UpstreamBody {
    fn new(response: &ResponseHead, buffer: BytesMut) -> Self {
        let length = response
            .headers
            .iter()
            .find_map(|(name, value)| match value {
                NodeHeaderValue::Single(value) if name == "content-length" => {
                    std::str::from_utf8(value).ok()?.trim().parse::<u64>().ok()
                }
                _ => None,
            });
        let framing =
            if matches!(response.status, 204 | 304) || (100..200).contains(&response.status) {
                Framing::Done
            } else if response.chunked {
                Framing::Chunked(ChunkState::Size)
            } else if let Some(length) = length {
                if length == 0 {
                    Framing::Done
                } else {
                    Framing::Length(length)
                }
            } else {
                Framing::UntilClose
            };
        Self { buffer, framing }
    }

    /// The next piece of body data, or `None` at its end.
    async fn next(
        &mut self,
        upstream: &mut (impl AsyncRead + Unpin),
    ) -> std::io::Result<Option<Bytes>> {
        loop {
            match self.framing {
                Framing::Done => return Ok(None),
                Framing::Length(remaining) => {
                    if self.buffer.is_empty() && self.fill(upstream).await? == 0 {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    let take = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(self.buffer.len());
                    let left = remaining - take as u64;
                    self.framing = if left == 0 {
                        Framing::Done
                    } else {
                        Framing::Length(left)
                    };
                    return Ok(Some(self.buffer.split_to(take).freeze()));
                }
                Framing::UntilClose => {
                    if self.buffer.is_empty() && self.fill(upstream).await? == 0 {
                        self.framing = Framing::Done;
                        return Ok(None);
                    }
                    return Ok(Some(self.buffer.split().freeze()));
                }
                Framing::Chunked(state) => {
                    if let Some(data) = self.chunked(state, upstream).await? {
                        return Ok(Some(data));
                    }
                }
            }
        }
    }

    /// Advances a chunked body by one step; returns data when a step produced some.
    async fn chunked(
        &mut self,
        state: ChunkState,
        upstream: &mut (impl AsyncRead + Unpin),
    ) -> std::io::Result<Option<Bytes>> {
        match state {
            ChunkState::Size | ChunkState::DataEnd | ChunkState::Trailers => {
                let line = self.line(upstream).await?;
                self.framing = match state {
                    ChunkState::Size => {
                        let size = line
                            .split(|&b| b == b';')
                            .next()
                            .and_then(|hex| std::str::from_utf8(hex).ok())
                            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
                            .ok_or_else(|| invalid("bad chunk size"))?;
                        if size == 0 {
                            Framing::Chunked(ChunkState::Trailers)
                        } else {
                            Framing::Chunked(ChunkState::Data(size))
                        }
                    }
                    ChunkState::DataEnd if line.is_empty() => Framing::Chunked(ChunkState::Size),
                    ChunkState::DataEnd => return Err(invalid("chunk without CRLF")),
                    _ if line.is_empty() => Framing::Done,
                    _ => Framing::Chunked(ChunkState::Trailers),
                };
                Ok(None)
            }
            ChunkState::Data(remaining) => {
                if self.buffer.is_empty() && self.fill(upstream).await? == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                let take = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(self.buffer.len());
                let left = remaining - take as u64;
                self.framing = Framing::Chunked(if left == 0 {
                    ChunkState::DataEnd
                } else {
                    ChunkState::Data(left)
                });
                Ok(Some(self.buffer.split_to(take).freeze()))
            }
        }
    }

    /// One line, without its line ending.
    async fn line(&mut self, upstream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
        loop {
            if let Some(at) = self.buffer.iter().position(|&b| b == b'\n') {
                let mut line = self.buffer.split_to(at + 1).to_vec();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.buffer.len() > MAX_RESPONSE_HEAD {
                return Err(invalid("chunk line too long"));
            }
            if self.fill(upstream).await? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
        }
    }

    async fn fill(&mut self, upstream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<usize> {
        self.buffer.reserve(16 * 1024);
        upstream.read_buf(&mut self.buffer).await
    }
}

/// Deviation D1 (WS-05): the rejection is answered with the error the HTTP path would give
/// (status, headers and JSON body of the error renderer) and the connection is closed.
async fn answer_and_close(shared: &Arc<Shared>, mut client: TcpStream, error: &GatewayError) {
    let bytes = closing_response(render_error(shared, error), shared.x_powered_by).await;
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
