//! Request targets as Node's HTTP parser accepts them (REQ-02, FWD-04).
//!
//! Node accepts `"`, `<`, `>`, `` ` ``, `{` and `}` unencoded in a request target and
//! rejects any byte above `0x7e` with 400. hyper's URI parser (the `http` crate) rejects some
//! of the former and accepts the latter. Before hyper parses a request head, the target is
//! rewritten: a byte the `http` crate rejects is percent-encoded, which changes nothing the
//! gateway does with it, since URL normalisation (REQ-02) and query re-serialisation (FWD-04)
//! encode those characters the same way; a byte above `0x7e` is replaced with a control
//! character, which makes hyper answer 400 as Node does.
//!
//! To find request heads in the byte stream, the request framing is followed: a head ends at
//! an empty line, its body is `Content-Length` bytes or chunked. After an upgrade or
//! `CONNECT`, or anything that cannot be followed, bytes pass through untouched.

use std::sync::OnceLock;

use bytes::BytesMut;

/// The longest request head that is rewritten; a longer one passes through untouched
/// (hyper rejects heads above its own limit anyway).
const MAX_HEAD: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for a request head.
    Head,
    /// Inside a body of known length: this many bytes remain.
    Length(u64),
    /// Inside a chunked body.
    Chunked(Chunk),
    /// No more rewriting on this connection.
    Opaque,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Reading a chunk-size line.
    Size,
    /// Inside chunk data: this many bytes remain, then CRLF.
    Data(u64),
    /// Reading trailer lines after the last chunk; true once a line had content.
    Trailers(bool),
}

/// Rewrites request heads in the bytes a client sends.
#[derive(Debug)]
pub(crate) struct Inbound {
    state: State,
    /// Bytes received but not yet processed.
    pending: BytesMut,
    /// The current line inside a chunked body (size line or trailer).
    line: Vec<u8>,
}

impl Default for Inbound {
    fn default() -> Self {
        Self {
            state: State::Head,
            pending: BytesMut::new(),
            line: Vec::new(),
        }
    }
}

impl Inbound {
    /// Feeds received bytes; returns the bytes to hand to hyper (possibly none yet, when a
    /// head is incomplete).
    pub(crate) fn feed(&mut self, received: &[u8], out: &mut BytesMut) {
        self.pending.extend_from_slice(received);
        loop {
            if self.pending.is_empty() {
                return;
            }
            match self.state {
                State::Opaque => {
                    out.extend_from_slice(&self.pending.split());
                    return;
                }
                State::Length(remaining) => {
                    let take = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .min(self.pending.len());
                    out.extend_from_slice(&self.pending.split_to(take));
                    let left = remaining - take as u64;
                    self.state = if left == 0 {
                        State::Head
                    } else {
                        State::Length(left)
                    };
                }
                State::Chunked(chunk) => self.chunked(chunk, out),
                State::Head => {
                    if !self.head(out) {
                        return;
                    }
                }
            }
        }
    }

    /// The connection ended: whatever is left goes to hyper as it is.
    pub(crate) fn finish(&mut self, out: &mut BytesMut) {
        out.extend_from_slice(&self.pending.split());
    }

    /// Processes one request head if it is complete. Returns false when more bytes are
    /// needed.
    fn head(&mut self, out: &mut BytesMut) -> bool {
        let Some(end) = find(&self.pending, b"\r\n\r\n") else {
            if self.pending.len() > MAX_HEAD {
                self.state = State::Opaque;
                return true;
            }
            return false;
        };
        let head = self.pending.split_to(end + 4);
        let (rewritten, next) = rewrite_head(&head);
        out.extend_from_slice(&rewritten);
        self.state = next;
        true
    }

    fn chunked(&mut self, chunk: Chunk, out: &mut BytesMut) {
        match chunk {
            Chunk::Size | Chunk::Trailers(_) => {
                let Some(at) = self.pending.iter().position(|&b| b == b'\n') else {
                    self.line.extend_from_slice(&self.pending);
                    if self.line.len() > MAX_HEAD {
                        self.state = State::Opaque;
                    }
                    out.extend_from_slice(&self.pending.split());
                    return;
                };
                let part = self.pending.split_to(at + 1);
                self.line.extend_from_slice(&part);
                out.extend_from_slice(&part);
                let line = std::mem::take(&mut self.line);
                let content = trim_line(&line);
                self.state = match chunk {
                    Chunk::Size => {
                        let size = content
                            .split(|&b| b == b';')
                            .next()
                            .and_then(|hex| std::str::from_utf8(hex).ok())
                            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok());
                        match size {
                            Some(0) => State::Chunked(Chunk::Trailers(false)),
                            Some(n) => State::Chunked(Chunk::Data(n.saturating_add(2))),
                            None => State::Opaque,
                        }
                    }
                    _ => {
                        if content.is_empty() {
                            State::Head
                        } else {
                            State::Chunked(Chunk::Trailers(true))
                        }
                    }
                };
            }
            Chunk::Data(remaining) => {
                let take = usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(self.pending.len());
                out.extend_from_slice(&self.pending.split_to(take));
                let left = remaining - take as u64;
                self.state = State::Chunked(if left == 0 {
                    Chunk::Size
                } else {
                    Chunk::Data(left)
                });
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn trim_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b'\r' | b'\n') {
        end -= 1;
    }
    &line[..end]
}

/// Bytes the `http` crate rejects in a request target, in the path and in the query.
fn rejected() -> &'static ([bool; 128], [bool; 128]) {
    static TABLES: OnceLock<([bool; 128], [bool; 128])> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut path = [false; 128];
        let mut query = [false; 128];
        for byte in 0x21u8..0x7f {
            if byte == b'%' || byte == b'?' || byte == b'#' {
                continue;
            }
            let c = char::from(byte);
            path[usize::from(byte)] =
                http::uri::PathAndQuery::try_from(format!("/a{c}b").as_str()).is_err();
            query[usize::from(byte)] =
                http::uri::PathAndQuery::try_from(format!("/a?b{c}d").as_str()).is_err();
        }
        (path, query)
    })
}

/// Rewrites the request line of a complete head and decides how its body is framed.
fn rewrite_head(head: &[u8]) -> (Vec<u8>, State) {
    // Empty lines before a request line are skipped by the parser.
    let start = head
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap_or(head.len());
    let Some(line_end) = find(&head[start..], b"\r\n").map(|i| i + start) else {
        return (head.to_vec(), State::Opaque);
    };
    let line = &head[start..line_end];
    let parts: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let mut out = head[..start].to_vec();
    let mut opaque = false;
    match parts.as_slice() {
        [method, target, version] if version.starts_with(b"HTTP/") => {
            out.extend_from_slice(method);
            out.push(b' ');
            out.extend_from_slice(&rewrite_target(target));
            out.push(b' ');
            out.extend_from_slice(version);
            opaque |= method.eq_ignore_ascii_case(b"CONNECT");
        }
        _ => {
            out.extend_from_slice(line);
            opaque = true;
        }
    }
    let fields = &head[line_end..];
    out.extend_from_slice(fields);

    let mut length: Option<u64> = None;
    let mut chunked = false;
    for field in fields.split(|&b| b == b'\n') {
        let field = trim_line(field);
        let Some(colon) = field.iter().position(|&b| b == b':') else {
            continue;
        };
        let name = &field[..colon];
        let value = String::from_utf8_lossy(&field[colon + 1..])
            .trim()
            .to_ascii_lowercase();
        if name.eq_ignore_ascii_case(b"content-length") {
            match value.parse::<u64>() {
                Ok(n) => length = Some(n),
                Err(_) => opaque = true,
            }
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            chunked = value
                .rsplit(',')
                .next()
                .is_some_and(|last| last.trim() == "chunked");
            if !chunked {
                opaque = true;
            }
        } else if name.eq_ignore_ascii_case(b"upgrade") {
            opaque = true;
        }
    }
    let state = if opaque {
        State::Opaque
    } else if chunked {
        State::Chunked(Chunk::Size)
    } else {
        match length {
            Some(n) if n > 0 => State::Length(n),
            _ => State::Head,
        }
    };
    (out, state)
}

/// Percent-encodes what the `http` crate rejects; replaces bytes above `0x7e` with a
/// control character so that the request is rejected with 400.
fn rewrite_target(target: &[u8]) -> Vec<u8> {
    let (path, query) = rejected();
    let mut out = Vec::with_capacity(target.len());
    let mut in_query = false;
    for &byte in target {
        if byte > 0x7e {
            out.push(0x01);
            continue;
        }
        if byte == b'?' {
            in_query = true;
        }
        let table = if in_query { query } else { path };
        if table[usize::from(byte)] {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut inbound = Inbound::default();
        let mut out = BytesMut::new();
        for chunk in chunks {
            inbound.feed(chunk, &mut out);
        }
        inbound.finish(&mut out);
        out.to_vec()
    }

    #[test]
    fn targets_are_rewritten_like_node_accepts_them() {
        assert_eq!(
            run(&[b"GET /svc/a\"b<c>d`e{f}?q=\"x\"<y> HTTP/1.1\r\nHost: a\r\n\r\n"]),
            b"GET /svc/a\"b%3Cc%3Ed%60e{f}?q=%22x%22%3Cy%3E HTTP/1.1\r\nHost: a\r\n\r\n".to_vec()
        );
        assert_eq!(
            run(&["GET /?q=\u{e9} HTTP/1.1\r\n\r\n".as_bytes()]),
            b"GET /?q=\x01\x01 HTTP/1.1\r\n\r\n".to_vec()
        );
    }

    #[test]
    fn bodies_are_left_alone_and_the_next_head_is_found() {
        let input: &[&[u8]] = &[
            b"POST /a HTTP/1.1\r\nContent-Length: 21\r\n\r\nGET /<x> HTT",
            b"P/1.1\r\n\r\nGET /<y> HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n<z>\r\n\r\n0\r\nX-T: 1\r\n\r\nGET /<w> HTTP/1.1\r\n\r\n",
        ];
        assert_eq!(
            String::from_utf8(run(input)).unwrap(),
            "POST /a HTTP/1.1\r\nContent-Length: 21\r\n\r\nGET /<x> HTTP/1.1\r\n\r\nGET /%3Cy%3E HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n<z>\r\n\r\n0\r\nX-T: 1\r\n\r\nGET /%3Cw%3E HTTP/1.1\r\n\r\n"
        );
    }

    #[test]
    fn upgrades_stop_the_rewriting() {
        assert_eq!(
            String::from_utf8(run(&[
                b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nGET /<x> HTTP/1.1\r\n\r\n"
            ]))
            .unwrap(),
            "GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nGET /<x> HTTP/1.1\r\n\r\n"
        );
    }
}
