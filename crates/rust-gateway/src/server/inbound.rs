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
//! `CONNECT`, or anything that cannot be followed, bytes pass through untouched. Heads are
//! delimited as hyper delimits them (lines may end with a bare LF) and read as Node reads
//! them: empty lines before a request line are dropped, and a head with a bare-LF line,
//! which Node refuses, is handed on behind a control character so that hyper refuses it too.
//!
//! When the server proxies WebSocket upgrades itself (area 06), an upgrade request head
//! ([`UpgradeHead`]) is not handed to hyper at all: it is held, with every byte after it,
//! until the server takes the connection over.

use std::sync::OnceLock;

use bytes::BytesMut;

use super::upgrade::{Held, UpgradeHead};

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
    /// An upgrade head was held back: nothing more is handed to hyper.
    Held,
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
    /// Whether upgrade heads are held back instead of handed to hyper.
    hold_upgrades: bool,
    /// The upgrade head held back.
    held: Option<UpgradeHead>,
}

impl Default for Inbound {
    fn default() -> Self {
        Self {
            state: State::Head,
            pending: BytesMut::new(),
            line: Vec::new(),
            hold_upgrades: false,
            held: None,
        }
    }
}

impl Inbound {
    /// An inbound stream that holds back upgrade heads (see the [module docs](self)).
    pub(crate) fn holding_upgrades() -> Self {
        Self {
            hold_upgrades: true,
            ..Self::default()
        }
    }

    /// Whether an upgrade head is held back.
    pub(crate) fn is_holding(&self) -> bool {
        self.state == State::Held
    }

    /// The upgrade head held back, and every byte received after it.
    pub(crate) fn take_held(&mut self) -> Option<Held> {
        let head = self.held.take()?;
        Some(Held {
            head,
            early: self.pending.split(),
        })
    }

    /// Feeds received bytes; returns the bytes to hand to hyper (possibly none yet, when a
    /// head is incomplete).
    pub(crate) fn feed(&mut self, received: &[u8], out: &mut BytesMut) {
        self.pending.extend_from_slice(received);
        loop {
            if self.pending.is_empty() {
                return;
            }
            match self.state {
                State::Held => return,
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
        if self.state != State::Held {
            out.extend_from_slice(&self.pending.split());
        }
    }

    /// Processes one request head if it is complete. Returns false when more bytes are
    /// needed.
    fn head(&mut self, out: &mut BytesMut) -> bool {
        // Empty lines before a request line: Node skips any run of CR and LF, hyper skips
        // CRLF and LF lines. Dropping them gives hyper what Node reads (FUZZ-05).
        let lead = self
            .pending
            .iter()
            .position(|&b| b != b'\r' && b != b'\n')
            .unwrap_or(self.pending.len());
        let _ = self.pending.split_to(lead);
        if self.pending.is_empty() {
            return false;
        }
        // The head ends at its first empty line, as hyper finds it: lines may end with CRLF
        // or a bare LF (FUZZ-01).
        let Some(end) = head_end(&self.pending) else {
            if self.pending.len() > MAX_HEAD {
                // Too long to rewrite whole; hyper applies its own limit. The request line
                // is still rewritten, or the request refused, so that no target reaches
                // hyper unrewritten.
                let pending = self.pending.split();
                match find(&pending, b"\n") {
                    Some(at) if !has_bare_lf(&pending[..=at]) => {
                        let (line, _) = rewrite_head(&pending[..=at]);
                        out.extend_from_slice(&line);
                        out.extend_from_slice(&pending[at + 1..]);
                    }
                    _ => refuse(&pending, out),
                }
                self.state = State::Opaque;
                return true;
            }
            return false;
        };
        // Node refuses a head with a line ended by a bare LF (400); hyper would read it.
        if has_bare_lf(&self.pending[..end]) {
            refuse(&self.pending.split(), out);
            self.state = State::Opaque;
            return true;
        }
        if self.hold_upgrades
            && let Some(upgrade) = UpgradeHead::parse(&self.pending[..end])
        {
            let _ = self.pending.split_to(end);
            self.held = Some(upgrade);
            self.state = State::Held;
            return true;
        }
        let head = self.pending.split_to(end);
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

/// The length of the head at the start of `bytes` (up to and including its empty line), as
/// `httparse` delimits it: lines end with CRLF or a bare LF.
fn head_end(bytes: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'\n' {
            continue;
        }
        if matches!(&bytes[line_start..i], b"" | b"\r") {
            return Some(i + 1);
        }
        line_start = i + 1;
    }
    None
}

/// Whether a line in `head` ends with a LF not preceded by CR.
fn has_bare_lf(head: &[u8]) -> bool {
    head.iter()
        .enumerate()
        .any(|(i, &b)| b == b'\n' && (i == 0 || head[i - 1] != b'\r'))
}

/// Hands hyper `head` with a control character in front, which it refuses with 400 before
/// reading anything else, as Node refuses the head.
fn refuse(head: &[u8], out: &mut BytesMut) {
    out.extend_from_slice(&[0x01]);
    out.extend_from_slice(head);
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
pub(crate) fn rewrite_target(target: &[u8]) -> Vec<u8> {
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
    fn upgrade_heads_are_held_with_what_follows() {
        let mut inbound = Inbound::holding_upgrades();
        let mut out = BytesMut::new();
        inbound.feed(
            b"GET /a HTTP/1.1\r\n\r\nGET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nearly",
            &mut out,
        );
        inbound.feed(b" bytes", &mut out);
        inbound.finish(&mut out);
        assert_eq!(&out[..], b"GET /a HTTP/1.1\r\n\r\n");
        assert!(inbound.is_holding());
        let held = inbound.take_held().expect("held");
        assert_eq!(held.head.target, "/ws");
        assert_eq!(&held.early[..], b"early bytes");

        // Without holding, and for heads that are not upgrades, nothing changes.
        let mut inbound = Inbound::holding_upgrades();
        let mut out = BytesMut::new();
        inbound.feed(b"GET /<x> HTTP/1.1\r\nUpgrade: h2c\r\n\r\n", &mut out);
        assert!(!inbound.is_holding());
        assert_eq!(&out[..], b"GET /%3Cx%3E HTTP/1.1\r\nUpgrade: h2c\r\n\r\n");
    }

    // FUZZ-01 (docs/security/fuzzing.md): hyper ends a head at a bare `\n\n` too. Node
    // refuses a head with a bare-LF line, so hyper is made to refuse it (400) instead of
    // reading it and the head after it unrewritten.
    #[test]
    fn fuzz_01_a_head_with_bare_lf_lines_is_refused() {
        let input = "GET /a HTTP/1.1\nHost: h\n\nGET /\u{e9} HTTP/1.1\r\nHost: h\r\n\r\n";
        let out = run(&[input.as_bytes()]);
        assert_eq!(out[0], 0x01, "{:?}", String::from_utf8_lossy(&out));
        assert_eq!(&out[1..], input.as_bytes());
        // Any bare LF in the head, as Node: the request line, a field, the empty line.
        for head in [
            "GET /a HTTP/1.1\r\nHost: h\r\n\n",
            "GET /a HTTP/1.1\r\nHost: h\nX: 1\r\n\r\n",
        ] {
            assert_eq!(run(&[head.as_bytes()])[0], 0x01, "{head:?}");
        }
        // Split anywhere, the result is the same.
        let bytes = input.as_bytes();
        for cut in 1..bytes.len() {
            assert_eq!(run(&[&bytes[..cut], &bytes[cut..]]), out, "cut at {cut}");
        }
    }

    // FUZZ-05: empty lines (any run of CR and LF, as Node reads them) before a request line
    // are dropped, and the request after them is rewritten as usual.
    #[test]
    fn fuzz_05_leading_empty_lines_are_skipped() {
        assert_eq!(
            run(&["\r\n\r\nGET /\u{e9} HTTP/1.1\r\nHost: h\r\n\r\n".as_bytes()]),
            b"GET /\x01\x01 HTTP/1.1\r\nHost: h\r\n\r\n".to_vec()
        );
        assert_eq!(
            run(&[
                b"\n\r\r",
                b"\nGET /<x> HTTP/1.1\r\n\r\n\r\nGET /<y> HTTP/1.1\r\n\r\n"
            ]),
            b"GET /%3Cx%3E HTTP/1.1\r\n\r\nGET /%3Cy%3E HTTP/1.1\r\n\r\n".to_vec()
        );
    }

    // A head longer than MAX_HEAD is left to hyper's own limit, but its request line is still
    // rewritten (or refused when it is not complete), never handed on raw.
    #[test]
    fn an_oversized_head_still_has_its_target_rewritten() {
        let mut head = "GET /\u{e9} HTTP/1.1\r\n".as_bytes().to_vec();
        head.extend(std::iter::repeat_n(b'x', MAX_HEAD + 10));
        let out = run(&[&head]);
        assert!(out.starts_with(b"GET /\x01\x01 HTTP/1.1\r\nxxx"));
        assert_eq!(out.len(), head.len());

        let mut line = b"GET /".to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_HEAD + 10));
        let out = run(&[&line]);
        assert_eq!(out[0], 0x01);
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
