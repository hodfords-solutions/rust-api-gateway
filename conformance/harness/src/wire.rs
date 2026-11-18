//! Minimal HTTP/1.1 wire handling shared by the mock upstream and the raw client.
//!
//! Both sides need byte-level control (exact header order, duplicates, raw request
//! targets, truncated bodies), so neither uses a high-level HTTP library.

use std::io;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Header list as it appeared on the wire: original name case, order and duplicates kept.
pub type Headers = Vec<(String, String)>;

/// First value of a header, matched case-insensitively.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Every value of a header, in order, matched case-insensitively.
pub fn header_all<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    /// The request target exactly as sent (path and query, undecoded).
    pub target: String,
    /// Minor HTTP version: 0 for HTTP/1.0, 1 for HTTP/1.1.
    pub version: u8,
    pub headers: Headers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub version: u8,
    pub status: u16,
    pub reason: String,
    pub headers: Headers,
}

fn collect_headers(raw: &[httparse::Header<'_>]) -> Headers {
    raw.iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect()
}

pub fn parse_request_head(bytes: &[u8]) -> Result<RequestHead> {
    let mut storage = [httparse::EMPTY_HEADER; 256];
    let mut req = httparse::Request::new(&mut storage);
    match req.parse(bytes).context("malformed request head")? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => bail!("incomplete request head"),
    }
    Ok(RequestHead {
        method: req.method.unwrap_or_default().to_string(),
        target: req.path.unwrap_or_default().to_string(),
        version: req.version.unwrap_or(1),
        headers: collect_headers(req.headers),
    })
}

pub fn parse_response_head(bytes: &[u8]) -> Result<ResponseHead> {
    let mut storage = [httparse::EMPTY_HEADER; 256];
    let mut res = httparse::Response::new(&mut storage);
    match res.parse(bytes).context("malformed response head")? {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => bail!("incomplete response head"),
    }
    Ok(ResponseHead {
        version: res.version.unwrap_or(1),
        status: res.code.unwrap_or_default(),
        reason: res.reason.unwrap_or_default().to_string(),
        headers: collect_headers(res.headers),
    })
}

/// A body read from the wire, and whether its framing completed before EOF.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Body {
    pub bytes: Vec<u8>,
    pub complete: bool,
}

/// A buffered reader over one connection.
pub struct WireReader<S> {
    stream: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + Unpin> WireReader<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    /// A reader whose buffer already holds `buffered`, read earlier from `stream`.
    pub fn with_buffered(stream: S, buffered: Vec<u8>) -> Self {
        Self {
            stream,
            buf: buffered,
        }
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Returns the stream and any bytes read past the last parsed message.
    pub fn into_parts(self) -> (S, Vec<u8>) {
        (self.stream, self.buf)
    }

    /// Takes the bytes read past the last parsed message.
    pub fn take_buffered(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    async fn fill(&mut self) -> io::Result<usize> {
        let mut chunk = [0u8; 16 * 1024];
        let n = self.stream.read(&mut chunk).await?;
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(n)
    }

    /// Reads up to and including the blank line ending a message head.
    /// `Ok(None)` means the peer closed the connection before sending anything.
    pub async fn read_head(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = find(&self.buf, b"\r\n\r\n") {
                let head: Vec<u8> = self.buf.drain(..end + 4).collect();
                return Ok(Some(head));
            }
            if self.fill().await? == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed inside a message head",
                ));
            }
        }
    }

    /// Reads `len` body bytes, or fewer if the peer closes first.
    pub async fn read_sized(&mut self, len: usize) -> io::Result<Body> {
        while self.buf.len() < len {
            if self.fill().await? == 0 {
                return Ok(Body {
                    bytes: std::mem::take(&mut self.buf),
                    complete: false,
                });
            }
        }
        Ok(Body {
            bytes: self.buf.drain(..len).collect(),
            complete: true,
        })
    }

    async fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(end) = find(&self.buf, b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..end + 2).take(end).collect();
                return Ok(Some(line));
            }
            if self.fill().await? == 0 {
                return Ok(None);
            }
        }
    }

    /// Decodes a chunked body. Stops early, with `complete: false`, if the peer closes.
    pub async fn read_chunked(&mut self) -> io::Result<Body> {
        let mut body = Vec::new();
        loop {
            let Some(line) = self.read_line().await? else {
                return Ok(Body {
                    bytes: body,
                    complete: false,
                });
            };
            let text = String::from_utf8_lossy(&line);
            let size_text = text.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_text, 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad chunk size {text:?}"),
                )
            })?;
            if size == 0 {
                // Trailers, then the final blank line.
                loop {
                    match self.read_line().await? {
                        Some(l) if l.is_empty() => {
                            return Ok(Body {
                                bytes: body,
                                complete: true,
                            });
                        }
                        Some(_) => continue,
                        None => {
                            return Ok(Body {
                                bytes: body,
                                complete: false,
                            });
                        }
                    }
                }
            }
            let chunk = self.read_sized(size + 2).await?;
            if !chunk.complete {
                body.extend_from_slice(&chunk.bytes);
                return Ok(Body {
                    bytes: body,
                    complete: false,
                });
            }
            body.extend_from_slice(&chunk.bytes[..size]);
        }
    }

    /// Reads until the peer closes the connection.
    pub async fn read_to_eof(&mut self) -> io::Result<Vec<u8>> {
        while self.fill().await? > 0 {}
        Ok(std::mem::take(&mut self.buf))
    }

    /// Reads a request body according to its headers.
    pub async fn read_request_body(&mut self, headers: &[(String, String)]) -> io::Result<Body> {
        if header(headers, "transfer-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
        {
            return self.read_chunked().await;
        }
        let len = header(headers, "content-length")
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        self.read_sized(len).await
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_head_keeping_order_case_and_duplicates() {
        let head = parse_request_head(
            b"GET /a/b?x=1&x=2 HTTP/1.1\r\nHost: h\r\nX-Dup: 1\r\nx-dup: 2\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/a/b?x=1&x=2");
        assert_eq!(head.version, 1);
        assert_eq!(
            head.headers,
            vec![
                ("Host".to_string(), "h".to_string()),
                ("X-Dup".to_string(), "1".to_string()),
                ("x-dup".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(header_all(&head.headers, "x-dup"), vec!["1", "2"]);
    }

    #[tokio::test]
    async fn reads_chunked_and_truncated_bodies() {
        let data: &[u8] = b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nTrailer: x\r\n\r\nrest";
        let mut reader = WireReader::new(data);
        let body = reader.read_chunked().await.unwrap();
        assert_eq!(body.bytes, b"hello world");
        assert!(body.complete);
        let (_, rest) = reader.into_parts();
        assert_eq!(rest, b"rest");

        let mut reader = WireReader::new(&b"5\r\nhel"[..]);
        let body = reader.read_chunked().await.unwrap();
        assert_eq!(body.bytes, b"hel");
        assert!(!body.complete);

        let mut reader = WireReader::new(&b"abc"[..]);
        let body = reader.read_sized(10).await.unwrap();
        assert_eq!(body.bytes, b"abc");
        assert!(!body.complete);
    }
}
