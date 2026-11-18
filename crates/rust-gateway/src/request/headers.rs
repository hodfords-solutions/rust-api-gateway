//! Header rules: excluded headers (REQ-03, REQ-17), the upstream request header list
//! (FWD-06 to FWD-12), its body framing (FWD-10, FWD-13), and the response header list
//! (FWD-15).
//!
//! The Reference Gateway reads request headers from Node's `req.headers`, which already
//! joins repeated headers, builds the upstream list in a `Map`, and hands it to undici, which
//! writes `host` first, adds `connection: keep-alive`, and treats `content-length` as body
//! framing. These functions reproduce each step; the HTTP client and server do the rest.

use std::net::IpAddr;

use http::Method;
use http::header::{HeaderMap, HeaderName, HeaderValue};

use crate::error::GatewayError;

use super::js_object::JsObject;

/// Headers never forwarded in either direction (FWD-06, FWD-15). Headers nominated by
/// `Connection` are forwarded.
pub const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// The `X-Powered-By` value Express puts first on every response.
pub const X_POWERED_BY: &str = "Express";

/// Headers whose repeated lines Node discards after the first.
const DROP_DUPLICATES: [&str; 18] = [
    "age",
    "authorization",
    "content-length",
    "content-type",
    "etag",
    "expires",
    "from",
    "host",
    "if-modified-since",
    "if-unmodified-since",
    "last-modified",
    "location",
    "max-forwards",
    "proxy-authorization",
    "referer",
    "retry-after",
    "server",
    "user-agent",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP_HEADERS.contains(&name)
}

/// Removes every `excludeHeaders` name from `headers`, matched case-insensitively (REQ-03,
/// and again before the middleware chain, REQ-17). Names that are not valid header names
/// cannot be present and are ignored.
pub fn strip_excluded_headers<S: AsRef<str>>(headers: &mut HeaderMap, exclude_headers: &[S]) {
    for name in exclude_headers {
        if let Ok(name) = HeaderName::from_bytes(name.as_ref().to_ascii_lowercase().as_bytes()) {
            headers.remove(&name);
        }
    }
}

/// A value of Node's `req.headers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeHeaderValue {
    /// One value: the first line for headers whose duplicates Node discards, the lines
    /// joined with `"; "` for `cookie`, or with `", "` for everything else.
    Single(Vec<u8>),
    /// `set-cookie`: every line, kept separate.
    Multiple(Vec<Vec<u8>>),
}

/// Client request headers in arrival order, as `(name, value)` pairs.
pub type ClientHeaders<'a> = &'a [(HeaderName, HeaderValue)];

/// Node's `req.headers` for the given request header lines: names lower-cased, values with
/// surrounding spaces and tabs trimmed, repeated lines discarded or joined (see
/// [`NodeHeaderValue`]), in JavaScript object key order (a header named like an array index,
/// such as `42`, comes first).
pub fn node_incoming_headers<'a>(
    lines: impl IntoIterator<Item = (&'a HeaderName, &'a HeaderValue)>,
) -> Vec<(String, NodeHeaderValue)> {
    node_headers_object(lines).into_entries()
}

fn trim_ows(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|b| !matches!(b, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|b| !matches!(b, b' ' | b'\t'))
        .map_or(start, |i| i + 1);
    &value[start..end.max(start)]
}

fn node_headers_object<'a>(
    lines: impl IntoIterator<Item = (&'a HeaderName, &'a HeaderValue)>,
) -> JsObject<NodeHeaderValue> {
    let mut object: JsObject<NodeHeaderValue> = JsObject::new();
    for (name, value) in lines {
        let name = name.as_str().to_ascii_lowercase();
        let value = trim_ows(value.as_bytes()).to_vec();
        match object.get_mut(&name) {
            None => {
                let entry = if name == "set-cookie" {
                    NodeHeaderValue::Multiple(vec![value])
                } else {
                    NodeHeaderValue::Single(value)
                };
                object.insert(name, entry);
            }
            Some(NodeHeaderValue::Multiple(values)) => values.push(value),
            Some(NodeHeaderValue::Single(existing)) => {
                if DROP_DUPLICATES.contains(&name.as_str()) {
                    continue;
                }
                existing.extend_from_slice(if name == "cookie" { b"; " } else { b", " });
                existing.extend_from_slice(&value);
            }
        }
    }
    object
}

/// The client address as Node reports it on the Reference Gateway's dual-stack listener
/// (FWD-07): an IPv4 peer is written as an IPv4-mapped IPv6 address (`::ffff:127.0.0.1`).
pub fn node_remote_address(peer: IpAddr) -> String {
    match peer {
        IpAddr::V4(v4) => format!("::ffff:{v4}"),
        IpAddr::V6(v6) => v6.to_string(),
    }
}

/// An insertion-ordered map, like the JavaScript `Map` the reference builds.
#[derive(Default)]
struct OrderedMap {
    entries: Vec<(String, NodeHeaderValue)>,
}

impl OrderedMap {
    fn set(&mut self, name: &str, value: NodeHeaderValue) {
        match self.entries.iter_mut().find(|(n, _)| n == name) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((name.to_string(), value)),
        }
    }
}

fn is_truthy(value: Option<&NodeHeaderValue>) -> bool {
    match value {
        Some(NodeHeaderValue::Single(v)) => !v.is_empty(),
        Some(NodeHeaderValue::Multiple(_)) => true,
        None => false,
    }
}

/// The header list the Reference Gateway gives its HTTP client for the upstream request
/// (FWD-06 to FWD-12), in order.
///
/// * `client`: the request headers in arrival order, after `excludeHeaders` were removed.
/// * `peer`: the client connection's address, formatted with [`node_remote_address`].
/// * `middleware`: the outgoing header set from
///   [`ProxyRequest::kebab_headers`](super::ProxyRequest::kebab_headers); empty for Static
///   and Bypass Requests.
///
/// Steps: Node's joining of the client's lines ([`node_incoming_headers`]); the
/// [`HOP_BY_HOP_HEADERS`] removed; `x-forwarded-for` set to the incoming value (when
/// non-empty) followed by `, ` and the peer, or to the peer alone; `content-length: 0` added
/// to `DELETE` and `OPTIONS` without one (undici then drops a zero length for methods that do
/// not expect a body, see [`upstream_framing`]); middleware headers set last, replacing a
/// client header of the same name in place or appended. `set-cookie` lines stay separate.
///
/// Fails with [`GatewayError::UpstreamUnavailable`] (503) where undici rejects the list: a
/// name that is not a token, a value with characters other than tab, printable ASCII and
/// bytes `0x80` to `0xff`, an `expect` header, or a `content-length` that is not digits.
pub fn upstream_request_headers(
    client: ClientHeaders<'_>,
    peer: &str,
    method: &Method,
    middleware: &[(String, String)],
) -> Result<Vec<(HeaderName, HeaderValue)>, GatewayError> {
    let incoming = node_headers_object(client.iter().map(|(n, v)| (n, v)));
    let incoming_xff = incoming.get("x-forwarded-for").cloned();
    let has_content_length = is_truthy(incoming.get("content-length"));
    let mut map = OrderedMap::default();
    for (name, value) in incoming.into_entries() {
        if !is_hop_by_hop(&name) {
            map.entries.push((name, value));
        }
    }
    let xff = match incoming_xff {
        Some(NodeHeaderValue::Single(mut value)) if !value.is_empty() => {
            value.extend_from_slice(b", ");
            value.extend_from_slice(peer.as_bytes());
            value
        }
        _ => peer.as_bytes().to_vec(),
    };
    map.set("x-forwarded-for", NodeHeaderValue::Single(xff));
    if (method == Method::DELETE || method == Method::OPTIONS) && !has_content_length {
        map.set("content-length", NodeHeaderValue::Single(b"0".to_vec()));
    }
    for (name, value) in middleware {
        let bytes = latin1(value).ok_or(GatewayError::UpstreamUnavailable)?;
        map.set(name, NodeHeaderValue::Single(bytes));
    }

    let mut out = Vec::new();
    for (name, value) in map.entries {
        let lower = name.to_ascii_lowercase();
        if lower == "expect" {
            return Err(GatewayError::UpstreamUnavailable);
        }
        let header_name = HeaderName::from_bytes(lower.as_bytes())
            .map_err(|_| GatewayError::UpstreamUnavailable)?;
        let values = match value {
            NodeHeaderValue::Single(v) => vec![v],
            NodeHeaderValue::Multiple(vs) => vs,
        };
        for v in values {
            if !is_valid_header_value(&v) || (lower == "content-length" && !is_digits(&v)) {
                return Err(GatewayError::UpstreamUnavailable);
            }
            let header_value =
                HeaderValue::from_bytes(&v).map_err(|_| GatewayError::UpstreamUnavailable)?;
            out.push((header_name.clone(), header_value));
        }
    }
    Ok(out)
}

/// A JavaScript string written as latin1 by undici: `None` when a character is above U+00FF.
fn latin1(value: &str) -> Option<Vec<u8>> {
    value
        .chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// undici's `isValidHeaderValue`: only tab, `0x20..=0x7e` and `0x80..=0xff`.
fn is_valid_header_value(value: &[u8]) -> bool {
    value
        .iter()
        .all(|&b| b == b'\t' || (0x20..=0x7e).contains(&b) || b >= 0x80)
}

fn is_digits(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(u8::is_ascii_digit)
}

/// What undici knows about the client's request body when it dispatches the upstream request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyLength {
    /// The client sent no body.
    Empty,
    /// The whole body has already arrived and has this many bytes (undici reads an ended
    /// stream's length, so a small chunked body is sent with a `content-length`).
    Known(u64),
    /// The body is still arriving.
    Streaming,
}

/// How the upstream request body is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// No `content-length` and no `transfer-encoding`.
    None,
    /// `content-length: n`.
    ContentLength(u64),
    /// `transfer-encoding: chunked`.
    Chunked,
}

/// Methods undici sends a `content-length: 0` for when the body is empty.
fn expects_payload(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "PUT" | "POST" | "PATCH" | "QUERY" | "PROPFIND" | "PROPPATCH"
    )
}

/// Methods for which undici checks a declared length against the body.
fn should_send_content_length(method: &Method) -> bool {
    !matches!(
        method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "TRACE" | "CONNECT"
    )
}

/// The body framing undici uses upstream (FWD-10, FWD-13). `declared` is the
/// `content-length` in the [`upstream_request_headers`] list, if any (including the zero the
/// gateway adds to `DELETE` and `OPTIONS`).
///
/// A zero length is not sent for methods that do not expect a payload, so a bodyless
/// `DELETE` or `OPTIONS` reaches the upstream without `content-length`, while `PUT`, `POST`,
/// `PATCH`, `QUERY`, `PROPFIND` and `PROPPATCH` get `content-length: 0`. A declared length
/// that disagrees with a known body length fails with
/// [`GatewayError::UpstreamUnavailable`] (503): a chunked `DELETE` with a body is one case.
pub fn upstream_framing(
    method: &Method,
    declared: Option<u64>,
    body: BodyLength,
) -> Result<Framing, GatewayError> {
    let known = match body {
        BodyLength::Empty => Some(0),
        BodyLength::Known(n) => Some(n),
        BodyLength::Streaming => None,
    };
    let mut length = known.or(declared);
    if length == Some(0) && !expects_payload(method) {
        length = None;
    }
    if let (Some(length), Some(declared)) = (length, declared)
        && should_send_content_length(method)
        && length > 0
        && declared != length
    {
        return Err(GatewayError::UpstreamUnavailable);
    }
    Ok(match (body, length) {
        (_, Some(n)) => Framing::ContentLength(n),
        (BodyLength::Streaming, None) => Framing::Chunked,
        (_, None) => Framing::None,
    })
}

/// The response headers the client gets from an upstream response (FWD-15), in order.
///
/// undici lower-cases names and groups repeated lines at the first one's position; the
/// [`HOP_BY_HOP_HEADERS`] are removed; Express's `x-powered-by: Express` comes first unless
/// the upstream sent its own `x-powered-by`, which then takes that place; `set-cookie` lines
/// come last, each kept separate.
///
/// The server adds what Node adds after these: `Date` when the upstream sent none,
/// `Connection: keep-alive` and `Keep-Alive: timeout=5` (or `Connection: close` when the
/// client asked to close), and `Transfer-Encoding: chunked` when there is no
/// `content-length` and the response has a body.
pub fn response_headers<'a>(
    upstream: impl IntoIterator<Item = (&'a HeaderName, &'a HeaderValue)>,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut groups: Vec<(HeaderName, Vec<HeaderValue>)> = Vec::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        match groups.iter_mut().find(|(n, _)| n == name) {
            Some((_, values)) => values.push(value.clone()),
            None => groups.push((name.clone(), vec![value.clone()])),
        }
    }
    let powered_by = HeaderName::from_static("x-powered-by");
    let mut out = Vec::new();
    match groups.iter().position(|(n, _)| *n == powered_by) {
        Some(at) => {
            let (name, values) = groups.remove(at);
            out.extend(values.into_iter().map(|v| (name.clone(), v)));
        }
        None => out.push((powered_by, HeaderValue::from_static(X_POWERED_BY))),
    }
    let mut cookies = Vec::new();
    for (name, values) in groups {
        if name == http::header::SET_COOKIE {
            cookies.extend(values);
        } else {
            out.extend(values.into_iter().map(|v| (name.clone(), v)));
        }
    }
    out.extend(cookies.into_iter().map(|v| (http::header::SET_COOKIE, v)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::golden;

    fn h(lines: &[(&str, &str)]) -> Vec<(HeaderName, HeaderValue)> {
        lines
            .iter()
            .map(|(n, v)| {
                (
                    HeaderName::from_bytes(n.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    fn text(list: &[(HeaderName, HeaderValue)]) -> Vec<String> {
        list.iter()
            .map(|(n, v)| format!("{n}: {}", String::from_utf8_lossy(v.as_bytes())))
            .collect()
    }

    fn upstream(
        lines: &[(&str, &str)],
        method: Method,
        middleware: &[(&str, &str)],
    ) -> Vec<String> {
        let middleware: Vec<(String, String)> = middleware
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect();
        text(&upstream_request_headers(&h(lines), "10.0.0.9", &method, &middleware).unwrap())
    }

    #[test]
    fn strips_excluded_headers_case_insensitively() {
        // request-sanitizer.helper.spec.ts
        let mut headers = HeaderMap::new();
        headers.insert("auth-user-id", HeaderValue::from_static("spoofed"));
        headers.insert("x-api-key", HeaderValue::from_static("k"));
        headers.insert("x-keep", HeaderValue::from_static("yes"));
        strip_excluded_headers(&mut headers, &["Auth-User-Id", "X-API-KEY", "not a name"]);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-keep"], "yes");
        let mut untouched = HeaderMap::new();
        untouched.insert("auth-user-id", HeaderValue::from_static("a"));
        strip_excluded_headers(&mut untouched, &[] as &[&str]);
        assert_eq!(untouched.len(), 1);
    }

    #[test]
    fn node_header_joining_matches_golden_vectors() {
        let mut failures = Vec::new();
        for case in golden::cases(golden::HEADERS) {
            let lines: Vec<(String, String)> = case[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| {
                    (
                        l[0].as_str().unwrap().to_string(),
                        l[1].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            if case[1].get("rejected").is_some() {
                continue; // Node's parser answers 400; the server's parser handles that.
            }
            let mut owned: Vec<(&str, &str)> = Vec::new();
            if !lines.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
                owned.push(("Host", "h"));
            }
            owned.extend(lines.iter().map(|(n, v)| (n.as_str(), v.as_str())));
            owned.push(("Connection", "close"));
            let input = h(&owned);
            let actual: Vec<serde_json::Value> =
                node_incoming_headers(input.iter().map(|(n, v)| (n, v)))
                    .into_iter()
                    .map(|(n, v)| match v {
                        NodeHeaderValue::Single(v) => {
                            serde_json::json!([n, String::from_utf8_lossy(&v)])
                        }
                        NodeHeaderValue::Multiple(vs) => serde_json::json!([
                            n,
                            vs.iter()
                                .map(|v| String::from_utf8_lossy(v).into_owned())
                                .collect::<Vec<_>>()
                        ]),
                    })
                    .collect();
            if serde_json::Value::Array(actual.clone()) != case[1] {
                failures.push(format!("{lines:?}: node {}, rust {actual:?}", case[1]));
            }
        }
        golden::assert_no_failures("headers.json", &failures);
    }

    // proxy-server.spec.ts
    #[test]
    fn removes_hop_by_hop_headers() {
        let list = upstream(
            &[
                ("Connection", "keep-alive"),
                ("transfer-encoding", "chunked"),
                ("upgrade", "websocket"),
                ("proxy-authorization", "secret"),
                ("content-type", "application/json"),
            ],
            Method::GET,
            &[],
        );
        assert_eq!(
            list,
            [
                "content-type: application/json",
                "x-forwarded-for: 10.0.0.9"
            ]
        );
    }

    #[test]
    fn x_forwarded_for() {
        let xff = |lines: &[(&str, &str)]| {
            upstream(lines, Method::GET, &[])
                .into_iter()
                .find(|l| l.starts_with("x-forwarded-for"))
                .unwrap()
        };
        assert_eq!(
            xff(&[("x-forwarded-for", "1.1.1.1")]),
            "x-forwarded-for: 1.1.1.1, 10.0.0.9"
        );
        assert_eq!(
            xff(&[
                ("x-forwarded-for", "1.1.1.1"),
                ("x-forwarded-for", "2.2.2.2")
            ]),
            "x-forwarded-for: 1.1.1.1, 2.2.2.2, 10.0.0.9"
        );
        assert_eq!(xff(&[]), "x-forwarded-for: 10.0.0.9");
        assert_eq!(xff(&[("x-forwarded-for", "")]), "x-forwarded-for: 10.0.0.9");
        assert_eq!(
            node_remote_address("127.0.0.1".parse().unwrap()),
            "::ffff:127.0.0.1"
        );
        assert_eq!(node_remote_address("::1".parse().unwrap()), "::1");
    }

    #[test]
    fn content_length_zero_for_delete_and_options() {
        for method in [Method::DELETE, Method::OPTIONS] {
            assert_eq!(
                upstream(&[], method, &[]),
                ["x-forwarded-for: 10.0.0.9", "content-length: 0"]
            );
        }
        assert_eq!(
            upstream(&[("content-length", "12")], Method::DELETE, &[]),
            ["content-length: 12", "x-forwarded-for: 10.0.0.9"]
        );
        assert_eq!(
            upstream(&[], Method::GET, &[]),
            ["x-forwarded-for: 10.0.0.9"]
        );
    }

    #[test]
    fn middleware_headers_come_last_and_override_in_place() {
        // Observed in forwarding::fwd_11_middleware_headers.
        let list = upstream(
            &[
                ("Host", "gw"),
                ("Accept", "client/accept"),
                ("X-Trace", "client"),
                ("X-Last", "client"),
            ],
            Method::GET,
            &[
                ("accept", "mw/accept"),
                ("x-trace", "mw"),
                ("x-new", "n"),
                ("auth-user-id", "123"),
            ],
        );
        assert_eq!(
            list,
            [
                "host: gw",
                "accept: mw/accept",
                "x-trace: mw",
                "x-last: client",
                "x-forwarded-for: 10.0.0.9",
                "x-new: n",
                "auth-user-id: 123",
            ]
        );
    }

    #[test]
    fn client_headers_are_joined_like_node() {
        // Observed in forwarding::fwd_12_client_headers.
        let list = upstream(
            &[
                ("Authorization", "Bearer first"),
                ("Cookie", "a=1"),
                ("X-Rep", "1"),
                ("Authorization", "Bearer second"),
                ("Cookie", "b=2"),
                ("X-Rep", "2"),
                ("Set-Cookie", "s=1"),
                ("Set-Cookie", "s=2"),
                ("X-Spaces", "  padded  "),
                ("42", "numeric name"),
            ],
            Method::GET,
            &[],
        );
        assert_eq!(
            list,
            [
                "42: numeric name",
                "authorization: Bearer first",
                "cookie: a=1; b=2",
                "x-rep: 1, 2",
                "set-cookie: s=1",
                "set-cookie: s=2",
                "x-spaces: padded",
                "x-forwarded-for: 10.0.0.9",
            ]
        );
    }

    #[test]
    fn what_undici_rejects_answers_503() {
        let reject = |lines: &[(&str, &str)], middleware: &[(&str, &str)]| {
            let middleware: Vec<(String, String)> = middleware
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
            matches!(
                upstream_request_headers(&h(lines), "::1", &Method::GET, &middleware),
                Err(GatewayError::UpstreamUnavailable)
            )
        };
        assert!(reject(&[("Expect", "100-continue")], &[]));
        assert!(reject(&[], &[("ü", "x")]));
        assert!(reject(&[], &[("x-bad", "a\nb")]));
        assert!(reject(&[], &[("x-wide", "日本")]));
        assert!(!reject(&[], &[("x-latin1", "é")]));
    }

    #[test]
    fn framing_as_observed_upstream() {
        // forwarding::fwd_10_content_length and fwd_06 (chunked POST fully received).
        use BodyLength::*;
        let f = |m: Method, declared, body| upstream_framing(&m, declared, body).ok();
        assert_eq!(f(Method::DELETE, Some(0), Empty), Some(Framing::None));
        assert_eq!(
            f(Method::DELETE, Some(4), Known(4)),
            Some(Framing::ContentLength(4))
        );
        assert_eq!(f(Method::DELETE, Some(0), Known(2)), None);
        assert_eq!(f(Method::OPTIONS, Some(0), Empty), Some(Framing::None));
        assert_eq!(f(Method::GET, None, Empty), Some(Framing::None));
        assert_eq!(f(Method::HEAD, None, Empty), Some(Framing::None));
        assert_eq!(f(Method::PUT, None, Empty), Some(Framing::ContentLength(0)));
        assert_eq!(
            f(Method::POST, None, Empty),
            Some(Framing::ContentLength(0))
        );
        assert_eq!(
            f(Method::POST, Some(2), Known(2)),
            Some(Framing::ContentLength(2))
        );
        assert_eq!(f(Method::TRACE, None, Empty), Some(Framing::None));
        let propfind = Method::from_bytes(b"PROPFIND").unwrap();
        assert_eq!(f(propfind, None, Empty), Some(Framing::ContentLength(0)));
        assert_eq!(
            f(Method::POST, None, Known(7)),
            Some(Framing::ContentLength(7))
        );
        assert_eq!(f(Method::POST, None, Streaming), Some(Framing::Chunked));
        assert_eq!(
            f(Method::POST, Some(16), Streaming),
            Some(Framing::ContentLength(16))
        );
    }

    #[test]
    fn response_headers_as_observed() {
        // forwarding::fwd_15_response_headers
        let list = text(&response_headers(
            h(&[
                ("X-One", "1"),
                ("Set-Cookie", "a=1; Path=/"),
                ("X-Dup", "1"),
                ("Set-Cookie", "b=2"),
                ("X-Dup", "2"),
                ("Keep-Alive", "timeout=9"),
                ("Proxy-Authenticate", "Basic"),
                ("Trailers", "X-T"),
                ("TE", "trailers"),
                ("Upgrade", "foo"),
                ("Connection", "X-One"),
                ("X-CaSe", "MiXed"),
                ("Content-Type", "text/plain"),
                ("content-length", "5"),
            ])
            .iter()
            .map(|(n, v)| (n, v)),
        ));
        assert_eq!(
            list,
            [
                "x-powered-by: Express",
                "x-one: 1",
                "x-dup: 1",
                "x-dup: 2",
                "x-case: MiXed",
                "content-type: text/plain",
                "content-length: 5",
                "set-cookie: a=1; Path=/",
                "set-cookie: b=2",
            ]
        );
        let own = text(&response_headers(
            h(&[("ETag", "\"abc\""), ("X-Powered-By", "Mock")])
                .iter()
                .map(|(n, v)| (n, v)),
        ));
        assert_eq!(own, ["x-powered-by: Mock", "etag: \"abc\""]);
    }
}
