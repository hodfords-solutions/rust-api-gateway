//! `node:querystring` ports used to re-serialise the forwarded query (FWD-04) and to read
//! `?type=` (documents), plus `kebab_case`. Properties: re-serialisation is a fixed point,
//! `escape` emits only unreserved characters and `%XX`, and `unescape(escape(s)) == s`.
#![no_main]

use api_gateway::documents::document_type;
use api_gateway::request::{
    forwarded_query, kebab_case, parse_query, query_escape, query_unescape, stringify_query,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let text = text.as_ref();

    let once = forwarded_query(text);
    let twice = forwarded_query(&once);
    assert_eq!(
        once, twice,
        "forwarded query is not a fixed point for {text:?}"
    );
    assert_eq!(stringify_query(&parse_query(text)), once);

    let escaped = query_escape(text);
    let bytes = escaped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            assert!(
                i + 2 < bytes.len()
                    && bytes[i + 1].is_ascii_hexdigit()
                    && bytes[i + 2].is_ascii_hexdigit(),
                "bad escape in {escaped:?}"
            );
            i += 3;
        } else {
            assert!(
                b.is_ascii_alphanumeric() || b"-._~!'()*".contains(&b),
                "escape left {:?} in {escaped:?}",
                char::from(b)
            );
            i += 1;
        }
    }
    assert_eq!(query_unescape(&escaped), text, "unescape(escape(s)) != s");
    let _ = query_unescape(text);

    let _ = kebab_case(text);
    let _ = document_type(Some(text));
});
