//! Header handling on the forwarding path: Node's `req.headers` view, the headers sent
//! upstream (FWD-05 to FWD-07), response headers, and `excludeHeaders` removal. Properties:
//! no hop-by-hop header is forwarded, `x-forwarded-for` ends with the peer, and response
//! headers keep every value of every non-hop-by-hop header.
//!
//! Input, one item per line: `method`, `peer`, then `name:value` header lines; a line
//! starting with `@` is a middleware header (`@name:value`), one starting with `-` an
//! `excludeHeaders` entry.
#![no_main]

use api_gateway::rate_limit::forwarded_for;
use api_gateway::request::{
    HOP_BY_HOP_HEADERS, node_incoming_headers, response_headers, strip_excluded_headers,
    upstream_request_headers,
};
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method};
use libfuzzer_sys::fuzz_target;

fn split_pair(line: &[u8]) -> (&[u8], &[u8]) {
    match line.iter().position(|&b| b == b':') {
        Some(at) => (&line[..at], &line[at + 1..]),
        None => (line, &[][..]),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut lines = data.split(|&b| b == b'\n');
    let method = Method::from_bytes(lines.next().unwrap_or(b"GET")).unwrap_or(Method::GET);
    let peer = String::from_utf8_lossy(lines.next().unwrap_or(b"::ffff:127.0.0.1")).into_owned();
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let mut middleware: Vec<(String, String)> = Vec::new();
    let mut exclude: Vec<String> = Vec::new();
    for line in lines.take(64) {
        if let Some(rest) = line.strip_prefix(b"@") {
            let (n, v) = split_pair(rest);
            middleware.push((
                String::from_utf8_lossy(n).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            ));
        } else if let Some(rest) = line.strip_prefix(b"-") {
            exclude.push(String::from_utf8_lossy(rest).into_owned());
        } else {
            let (n, v) = split_pair(line);
            if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(n), HeaderValue::from_bytes(v)) {
                headers.push((n, v));
            }
        }
    }
    let _ = node_incoming_headers(headers.iter().map(|(n, v)| (n, v)));

    if let Ok(out) = upstream_request_headers(&headers, &peer, &method, &middleware)
        && middleware.is_empty()
    {
        for (name, _) in &out {
            assert!(
                !HOP_BY_HOP_HEADERS.contains(&name.as_str()),
                "hop-by-hop header {name} forwarded"
            );
        }
        let xff: Vec<_> = out.iter().filter(|(n, _)| n == "x-forwarded-for").collect();
        assert_eq!(xff.len(), 1, "one x-forwarded-for expected");
        assert!(
            xff[0].1.as_bytes().ends_with(peer.as_bytes()),
            "x-forwarded-for does not end with the peer"
        );
    }

    let response = response_headers(headers.iter().map(|(n, v)| (n, v)));
    let kept = headers
        .iter()
        .filter(|(n, _)| !HOP_BY_HOP_HEADERS.contains(&n.as_str()))
        .count();
    let powered_by_added = !headers.iter().any(|(n, _)| n == "x-powered-by");
    assert_eq!(response.len(), kept + usize::from(powered_by_added));

    let mut map = HeaderMap::new();
    for (name, value) in &headers {
        map.append(name.clone(), value.clone());
    }
    let _ = forwarded_for(&map);
    strip_excluded_headers(&mut map, &exclude);
    for name in &exclude {
        assert!(
            map.get(name.to_ascii_lowercase().as_str()).is_none(),
            "excluded header {name:?} left in"
        );
    }
});
