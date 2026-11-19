//! Helpers shared by the fuzz targets in `fuzz_targets/`.

use std::sync::OnceLock;

/// Whether the open finding `id` (`FUZZ-NN`, see docs/security/fuzzing.md) is listed in the
/// comma-separated `FUZZ_SKIP_OPEN_FINDINGS` environment variable. Targets then step around
/// the narrow input shape that reproduces it, so CI keeps looking for new problems instead
/// of rediscovering a documented one. Unset (the default for local runs), nothing is skipped.
pub fn skip_open_finding(id: &str) -> bool {
    static SKIPPED: OnceLock<Vec<String>> = OnceLock::new();
    SKIPPED
        .get_or_init(|| {
            std::env::var("FUZZ_SKIP_OPEN_FINDINGS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .iter()
        .any(|s| s == id)
}

/// Whether `bytes` has a line feed not preceded by a carriage return.
pub fn has_bare_lf(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\n' && (i == 0 || bytes[i - 1] != b'\r'))
}

/// Whether a path segment is a dot segment as the WHATWG URL parser understands it
/// (`.`, `..`, and their `%2e` spellings in any case).
pub fn is_dot_segment(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "." | ".." | "%2e" | "%2e%2e" | ".%2e" | "%2e."
    )
}

/// The dot segments of the path part (before `?` and `#`) of `target`.
pub fn dot_segments(target: &str) -> Vec<&str> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    path.split('/').filter(|s| is_dot_segment(s)).collect()
}

/// JavaScript's `encodeURIComponent` for a string (UTF-8, upper-case hex).
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A request head as hyper's HTTP/1 server would see it in a byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenHead {
    /// Byte offset of the head in the stream.
    pub offset: usize,
    /// The method.
    pub method: String,
    /// The request target, as bytes.
    pub target: Vec<u8>,
}

/// Follows `stream` the way hyper's HTTP/1 server reads requests from a connection
/// (`httparse` for heads, `Content-Length` or chunked bodies) and returns every head it
/// would act on. Stops at the first point where hyper would answer an error and close, or
/// would stop reading requests (upgrade, `CONNECT`, `Connection: close`, HTTP/1.0 without
/// keep-alive), or where the stream ends.
///
/// This is deliberately conservative: anything ambiguous ends the walk, so a head reported
/// here is one hyper certainly parses.
pub fn hyper_heads(stream: &[u8]) -> Vec<SeenHead> {
    let mut seen = Vec::new();
    let mut pos = 0;
    while pos < stream.len() {
        let mut headers = [httparse::EMPTY_HEADER; 100];
        let mut request = httparse::Request::new(&mut headers);
        let Ok(httparse::Status::Complete(head_len)) = request.parse(&stream[pos..]) else {
            break;
        };
        let (Some(method), Some(target), Some(version)) =
            (request.method, request.path, request.version)
        else {
            break;
        };
        // hyper parses the target with the `http` crate and answers 400 when that fails.
        if http::Uri::try_from(target.as_bytes()).is_err() {
            break;
        }
        seen.push(SeenHead {
            offset: pos,
            method: method.to_string(),
            target: target.as_bytes().to_vec(),
        });
        if method.eq_ignore_ascii_case("CONNECT") {
            break;
        }
        let mut lengths = Vec::new();
        let mut transfer_encoding: Option<Vec<u8>> = None;
        let mut keep_alive = version == 1;
        let mut upgrade = false;
        for header in request.headers.iter() {
            let name = header.name.to_ascii_lowercase();
            match name.as_str() {
                "content-length" => lengths.push(header.value.to_vec()),
                "transfer-encoding" => match &mut transfer_encoding {
                    Some(te) => {
                        te.extend_from_slice(b",");
                        te.extend_from_slice(header.value);
                    }
                    None => transfer_encoding = Some(header.value.to_vec()),
                },
                "connection" => {
                    let value = String::from_utf8_lossy(header.value).to_ascii_lowercase();
                    for token in value.split(',').map(str::trim) {
                        match token {
                            "close" => keep_alive = false,
                            "keep-alive" => keep_alive = true,
                            "upgrade" => upgrade = true,
                            _ => {}
                        }
                    }
                }
                "upgrade" => upgrade = true,
                _ => {}
            }
        }
        if upgrade || !keep_alive {
            break;
        }
        pos += head_len;
        if let Some(te) = transfer_encoding {
            let te = String::from_utf8_lossy(&te).to_ascii_lowercase();
            if version == 0 || te.rsplit(',').next().map(str::trim) != Some("chunked") {
                break;
            }
            match chunked_body_len(&stream[pos..]) {
                Some(len) => pos += len,
                None => break,
            }
        } else if !lengths.is_empty() {
            let Some(length) = content_length(&lengths) else {
                break;
            };
            let Ok(length) = usize::try_from(length) else {
                break;
            };
            if stream.len() - pos < length {
                break;
            }
            pos += length;
        }
    }
    seen
}

/// hyper's reading of `Content-Length` lines: every value, split on commas, must be the same
/// decimal number.
fn content_length(values: &[Vec<u8>]) -> Option<u64> {
    let mut result: Option<u64> = None;
    for value in values {
        for part in value.split(|&b| b == b',') {
            let part = part.trim_ascii();
            if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let n: u64 = std::str::from_utf8(part).ok()?.parse().ok()?;
            match result {
                Some(existing) if existing != n => return None,
                _ => result = Some(n),
            }
        }
    }
    result
}

/// The length of a complete chunked body at the start of `bytes`, trailers included, or
/// `None` when it is incomplete or malformed.
fn chunked_body_len(bytes: &[u8]) -> Option<usize> {
    let mut pos = 0;
    loop {
        let httparse::Status::Complete((consumed, size)) =
            httparse::parse_chunk_size(&bytes[pos..]).ok()?
        else {
            return None;
        };
        pos += consumed;
        if size == 0 {
            break;
        }
        let size = usize::try_from(size).ok()?;
        if bytes.len() - pos < size + 2 || &bytes[pos + size..pos + size + 2] != b"\r\n" {
            return None;
        }
        pos += size + 2;
    }
    // Trailers: header lines up to an empty line.
    loop {
        let rest = &bytes[pos..];
        let end = rest.windows(2).position(|w| w == b"\r\n")?;
        pos += end + 2;
        if end == 0 {
            return Some(pos);
        }
    }
}
