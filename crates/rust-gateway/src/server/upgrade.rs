//! Upgrade requests, recognised the way Node's HTTP parser recognises them (WS-01).
//!
//! Node emits its `upgrade` event, which the Reference Gateway handles apart from Express,
//! for a request whose head it accepts and that has an `Upgrade` header and a `Connection`
//! header listing the `upgrade` token. `CONNECT` is Node's `connect` event instead, which
//! the reference does not handle. A head Node would reject is not recognised here: it goes
//! to hyper, which answers 400 as Node does.

use bytes::{Bytes, BytesMut};
use http::header::{HeaderName, HeaderValue};
use http::{Method, Version};
use tokio::net::TcpStream;

/// The methods Node's parser accepts (llhttp), apart from `CONNECT`.
const NODE_METHODS: [&str; 34] = [
    "DELETE",
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "OPTIONS",
    "TRACE",
    "COPY",
    "LOCK",
    "MKCOL",
    "MOVE",
    "PROPFIND",
    "PROPPATCH",
    "SEARCH",
    "UNLOCK",
    "BIND",
    "REBIND",
    "UNBIND",
    "ACL",
    "REPORT",
    "MKACTIVITY",
    "CHECKOUT",
    "MERGE",
    "M-SEARCH",
    "NOTIFY",
    "SUBSCRIBE",
    "UNSUBSCRIBE",
    "PATCH",
    "PURGE",
    "MKCALENDAR",
    "LINK",
    "UNLINK",
    "SOURCE",
    "QUERY",
];

/// The head of an upgrade request, as received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UpgradeHead {
    /// The request method.
    pub(crate) method: Method,
    /// The request target, exactly as sent (printable ASCII only).
    pub(crate) target: String,
    /// `HTTP/1.0` or `HTTP/1.1`.
    pub(crate) version: Version,
    /// The header lines in arrival order, names lower-cased.
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
}

impl UpgradeHead {
    /// Parses a complete request head (up to and including its empty line). `None` unless
    /// Node would accept it and emit `upgrade` for it.
    pub(crate) fn parse(head: &[u8]) -> Option<Self> {
        let mut storage = vec![httparse::EMPTY_HEADER; 128];
        let mut request = httparse::Request::new(&mut storage);
        let parsed = match request.parse(head) {
            Err(httparse::Error::TooManyHeaders) => {
                // Node accepts up to 2000 header lines.
                storage = vec![httparse::EMPTY_HEADER; 2000];
                request = httparse::Request::new(&mut storage);
                request.parse(head)
            }
            other => other,
        };
        if !matches!(parsed, Ok(httparse::Status::Complete(_))) {
            return None;
        }
        let method = request.method?;
        if !NODE_METHODS.contains(&method) {
            return None;
        }
        let target = request.path?;
        if target.is_empty() || !target.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return None;
        }
        let version = match request.version? {
            0 => Version::HTTP_10,
            1 => Version::HTTP_11,
            _ => return None,
        };
        let mut headers = Vec::with_capacity(request.headers.len());
        let mut has_upgrade = false;
        let mut connection_upgrade = false;
        for header in request.headers.iter() {
            let name = HeaderName::from_bytes(header.name.as_bytes()).ok()?;
            let value = HeaderValue::from_bytes(header.value).ok()?;
            if name == http::header::UPGRADE {
                has_upgrade = true;
            } else if name == http::header::CONNECTION {
                connection_upgrade |= value
                    .as_bytes()
                    .split(|&b| b == b',')
                    .any(|token| token.trim_ascii().eq_ignore_ascii_case(b"upgrade"));
            }
            headers.push((name, value));
        }
        if !(has_upgrade && connection_upgrade) {
            return None;
        }
        Some(Self {
            method: Method::from_bytes(method.as_bytes()).ok()?,
            target: target.to_string(),
            version,
            headers,
        })
    }
}

/// An upgrade request taken over from the HTTP stack: its head, the client connection, and
/// the bytes the client sent after the head.
#[derive(Debug)]
pub(crate) struct Takeover {
    /// The upgrade request.
    pub(crate) head: UpgradeHead,
    /// The client connection, with nothing written on it for this request.
    pub(crate) stream: TcpStream,
    /// Bytes received after the head (the start of the upgraded protocol, WS-12).
    pub(crate) early: Bytes,
}

/// An upgrade head held back from hyper, and the bytes that followed it.
#[derive(Debug)]
pub(crate) struct Held {
    pub(crate) head: UpgradeHead,
    pub(crate) early: BytesMut,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(head: &str) -> Option<UpgradeHead> {
        UpgradeHead::parse(head.as_bytes())
    }

    #[test]
    fn needs_an_upgrade_header_and_the_connection_token() {
        let head = parse(
            "GET /svc/socket?x=1 HTTP/1.1\r\nHost: a\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .expect("upgrade");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.target, "/svc/socket?x=1");
        assert_eq!(head.version, Version::HTTP_11);
        let names: Vec<&str> = head.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["host", "connection", "upgrade"]);

        assert!(parse("GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n").is_none());
        assert!(parse("GET / HTTP/1.1\r\nConnection: upgrade\r\n\r\n").is_none());
        assert!(parse("GET / HTTP/1.1\r\nConnection: upgraded\r\nUpgrade: x\r\n\r\n").is_none());
        // Any value of `Upgrade`, any method Node knows, HTTP/1.0 too.
        assert!(parse("POST / HTTP/1.0\r\nconnection: UPGRADE\r\nupgrade: h2c\r\n\r\n").is_some());
    }

    #[test]
    fn heads_node_rejects_or_routes_elsewhere_are_not_taken_over() {
        let upgrade = "\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n";
        for line in [
            "CONNECT a:443 HTTP/1.1",
            "get / HTTP/1.1",
            "FOO / HTTP/1.1",
            "GET /\u{e9} HTTP/1.1",
            "GET / HTTP/2.0",
        ] {
            assert!(parse(&format!("{line}{upgrade}")).is_none(), "{line}");
        }
        assert!(
            parse("GET / HTTP/1.1\r\nBad Name: x\r\nConnection: Upgrade\r\nUpgrade: x\r\n\r\n")
                .is_none()
        );
        assert!(parse("GET / HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: x\r\n").is_none());
    }

    #[test]
    fn many_headers_are_accepted() {
        let mut head =
            String::from("GET / HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n");
        for i in 0..500 {
            head.push_str(&format!("x-h{i}: {i}\r\n"));
        }
        head.push_str("\r\n");
        assert_eq!(parse(&head).map(|h| h.headers.len()), Some(502));
    }
}
