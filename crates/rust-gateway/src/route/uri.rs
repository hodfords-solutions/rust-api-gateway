//! The two JavaScript URL helpers Route matching depends on.

/// `decodeURIComponent` failed (`URIError: URI malformed` in JavaScript).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("URI malformed")]
pub struct UriError;

/// A request URL that `new URL()` rejects (`TypeError: Invalid URL` in JavaScript).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid URL `{url}`")]
pub struct InvalidUrl {
    /// The rejected value.
    pub url: String,
}

/// JavaScript's `decodeURIComponent`: decodes every `%XX` escape as UTF-8. A `%` not
/// followed by two hex digits, or escapes that do not form valid UTF-8 (truncated,
/// overlong, surrogate or out-of-range sequences), are errors.
pub fn decode_uri_component(value: &str) -> Result<String, UriError> {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        out.push_str(&value[plain_start..i]);
        // A run of consecutive escapes must be valid UTF-8 on its own: JavaScript reads a
        // multi-byte sequence only from escapes that follow each other directly.
        let mut run = Vec::new();
        while i < bytes.len() && bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or(UriError)?;
            let high = hex_value(hex[0]).ok_or(UriError)?;
            let low = hex_value(hex[1]).ok_or(UriError)?;
            run.push(high << 4 | low);
            i += 3;
        }
        out.push_str(std::str::from_utf8(&run).map_err(|_| UriError)?);
        plain_start = i;
    }
    out.push_str(&value[plain_start..]);
    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The reference's `getPathFromUrl`: the WHATWG `pathname` of `value` when it is an
/// absolute URL, otherwise of `http://localhost` followed by `value`.
///
/// URL parsing drops the query and fragment, resolves `.` and `..` segments (also
/// percent-encoded ones), turns `\` into `/`, and percent-encodes spaces, quotes, angle
/// brackets, backticks, braces and non-ASCII characters, so the result is ASCII.
pub fn path_from_url(value: &str) -> Result<String, InvalidUrl> {
    let url = url::Url::parse(value)
        .or_else(|_| url::Url::parse(&format!("http://localhost{value}")))
        .map_err(|_| InvalidUrl {
            url: value.to_string(),
        })?;
    Ok(pathname(&url))
}

/// `url.pathname` as Node computes it. The WHATWG URL Standard added `^` to the path
/// percent-encode set, which the `url` crate does not encode yet; opaque paths
/// (`mailto:x`) keep it.
fn pathname(url: &url::Url) -> String {
    let path = url.path();
    if url.cannot_be_a_base() {
        path.to_string()
    } else {
        path.replace('^', "%5E")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_like_decode_uri_component() {
        assert_eq!(decode_uri_component("a%2Fb").unwrap(), "a/b");
        assert_eq!(decode_uri_component("caf%C3%A9").unwrap(), "café");
        assert_eq!(decode_uri_component("%F0%9F%98%80").unwrap(), "😀");
        assert_eq!(decode_uri_component("a+b%20c").unwrap(), "a+b c");
        assert_eq!(decode_uri_component("%41%42").unwrap(), "AB");
        assert_eq!(decode_uri_component("é").unwrap(), "é");
        for bad in [
            "%",
            "%z",
            "%zz",
            "%4",
            "%C3",
            "%C3a",
            "%C3%41",
            "%C0%AF",
            "%ED%A0%80",
            "%F4%90%80%80",
            "%A9",
            "%E0%A4%A",
        ] {
            assert_eq!(decode_uri_component(bad), Err(UriError), "{bad}");
        }
    }

    // Ported from `libs/api-gateway/restful/helpers/string.helper.spec.ts` (`getPathFromUrl`).
    #[test]
    fn oas_18_extracts_the_path_from_an_absolute_url() {
        assert_eq!(
            path_from_url("http://example.com/users/1?active=true").unwrap(),
            "/users/1"
        );
    }

    #[test]
    fn oas_18_extracts_the_path_from_a_relative_url() {
        assert_eq!(path_from_url("/users/1?active=true#x").unwrap(), "/users/1");
        assert_eq!(path_from_url("/users/1").unwrap(), "/users/1");
    }

    #[test]
    fn oas_18_returns_a_slash_for_a_root_url() {
        assert_eq!(path_from_url("http://example.com").unwrap(), "/");
        assert_eq!(path_from_url("/").unwrap(), "/");
    }

    #[test]
    fn oas_18_normalises_the_path_like_whatwg_url() {
        assert_eq!(path_from_url("/a/../b").unwrap(), "/b");
        assert_eq!(path_from_url("/a/%2e%2E/b").unwrap(), "/b");
        assert_eq!(path_from_url("/a/./b").unwrap(), "/a/b");
        assert_eq!(path_from_url("/a\\b").unwrap(), "/a/b");
        assert_eq!(
            path_from_url("/a b\"<>`{}").unwrap(),
            "/a%20b%22%3C%3E%60%7B%7D"
        );
        assert_eq!(path_from_url("/café").unwrap(), "/caf%C3%A9");
        assert_eq!(path_from_url("/a%2Fb;c").unwrap(), "/a%2Fb;c");
        assert_eq!(path_from_url("//host/x").unwrap(), "//host/x");
        assert_eq!(path_from_url("/a|b^c[d]").unwrap(), "/a|b%5Ec[d]");
        assert_eq!(path_from_url("/%zz").unwrap(), "/%zz");
        assert_eq!(path_from_url("mailto:x").unwrap(), "x");
        assert_eq!(path_from_url("x").unwrap(), "/");
        assert!(path_from_url(":99999").is_err());
    }

    /// Node's `new URL("http://localhost/a" + c + "b").pathname` for every ASCII `c` it
    /// changes; every other character is kept.
    #[test]
    fn oas_18_ascii_characters_are_encoded_like_node() {
        let changed: &[(u8, &str)] = &[
            (0, "/a%00b"),
            (1, "/a%01b"),
            (2, "/a%02b"),
            (3, "/a%03b"),
            (4, "/a%04b"),
            (5, "/a%05b"),
            (6, "/a%06b"),
            (7, "/a%07b"),
            (8, "/a%08b"),
            (9, "/ab"),
            (10, "/ab"),
            (11, "/a%0Bb"),
            (12, "/a%0Cb"),
            (13, "/ab"),
            (14, "/a%0Eb"),
            (15, "/a%0Fb"),
            (16, "/a%10b"),
            (17, "/a%11b"),
            (18, "/a%12b"),
            (19, "/a%13b"),
            (20, "/a%14b"),
            (21, "/a%15b"),
            (22, "/a%16b"),
            (23, "/a%17b"),
            (24, "/a%18b"),
            (25, "/a%19b"),
            (26, "/a%1Ab"),
            (27, "/a%1Bb"),
            (28, "/a%1Cb"),
            (29, "/a%1Db"),
            (30, "/a%1Eb"),
            (31, "/a%1Fb"),
            (32, "/a%20b"),
            (34, "/a%22b"),
            (35, "/a"),
            (60, "/a%3Cb"),
            (62, "/a%3Eb"),
            (63, "/a"),
            (92, "/a/b"),
            (94, "/a%5Eb"),
            (96, "/a%60b"),
            (123, "/a%7Bb"),
            (125, "/a%7Db"),
            (127, "/a%7Fb"),
        ];
        for byte in 0u8..128 {
            let c = char::from(byte);
            let expected = changed
                .iter()
                .find(|(b, _)| *b == byte)
                .map_or_else(|| format!("/a{c}b"), |(_, p)| (*p).to_string());
            assert_eq!(
                path_from_url(&format!("/a{c}b")).unwrap(),
                expected,
                "byte {byte}"
            );
        }
        assert_eq!(path_from_url("foo://h/a^b").unwrap(), "/a%5Eb");
        assert_eq!(path_from_url("mailto:a^b").unwrap(), "a^b");
    }
}
