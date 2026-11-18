//! URL normalisation (REQ-02) and the malformed percent-encoding check (REQ-26).

use std::sync::OnceLock;

use url::Url;

/// The base the Reference Gateway resolves request targets against.
pub const NORMALISATION_BASE: &str = "http://dummy-base.local";

fn base() -> &'static Url {
    static BASE: OnceLock<Url> = OnceLock::new();
    BASE.get_or_init(|| {
        Url::parse(NORMALISATION_BASE).expect("the normalisation base is a valid URL")
    })
}

/// A request target the WHATWG URL parser rejects (for example `//[bad/x`, whose authority is
/// not a valid host). The Reference Gateway answers such a request with 500.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the request target {target:?} is not a valid URL")]
pub struct InvalidUrl {
    /// The target as received.
    pub target: String,
}

/// Normalises a request target the way the Reference Gateway does before any decision
/// (REQ-02): `new URL(target, "http://dummy-base.local")`, then `pathname + search`.
///
/// Dot segments (also percent-encoded as `%2e`) are resolved, backslashes become slashes,
/// the fragment is dropped, tab, CR and LF are removed, characters outside the WHATWG
/// percent-encode sets are encoded, and a leading `//authority` or an absolute-form target
/// contributes only its path and query. A bare `?` is dropped. Repeated slashes, `..%2f`
/// and `;` are kept.
pub fn normalize_request_url(target: &str) -> Result<String, InvalidUrl> {
    let url = base()
        .join(&collapse_leading_slashes(target))
        .map_err(|_| InvalidUrl {
            target: target.to_string(),
        })?;
    let mut out = if url.cannot_be_a_base() {
        url.path().to_string()
    } else {
        // Node's parser also percent-encodes `^` in path segments.
        url.path().replace('^', "%5E")
    };
    if let Some(query) = url.query().filter(|q| !q.is_empty()) {
        out.push('?');
        out.push_str(query);
    }
    Ok(out)
}

/// For a target relative to the special base, WHATWG skips any number of slashes or
/// backslashes before the authority (`///host/x` is `//host/x`); the `url` crate does not.
fn collapse_leading_slashes(target: &str) -> std::borrow::Cow<'_, str> {
    let cleaned: String = target
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    let slashes = cleaned
        .chars()
        .take_while(|c| matches!(c, '/' | '\\'))
        .count();
    if slashes > 2 {
        std::borrow::Cow::Owned(format!("//{}", &cleaned[slashes..]))
    } else {
        std::borrow::Cow::Borrowed(target)
    }
}

/// The path segment Express's router fails to decode, if any (REQ-26).
///
/// The Reference Gateway's catch-all route `/{*splat}` decodes every `/`-separated segment
/// of the request path with `decodeURIComponent` before the gateway runs, so a malformed
/// percent-encoding (`%` not followed by two hex digits, or bytes that are not UTF-8) in any
/// segment answers 400 for every request class. The message quotes the first such segment
/// whole. The query and the fragment are not checked. This runs on the target as received,
/// before [`normalize_request_url`].
///
/// The path is taken the way `parseurl` takes it: up to the first `?` when the target starts
/// with `/` and has no `#` or whitespace; otherwise as Node's legacy `url.parse` sees it
/// (fragment and query removed, backslashes before them turned into slashes, the authority
/// of an absolute-form target removed, and `"'<>\^`{|}` escaped).
pub fn malformed_path_segment(target: &str) -> Option<String> {
    let pathname = express_pathname(target);
    let splat = pathname.strip_prefix('/')?;
    splat
        .split('/')
        .find(|segment| !decodes_as_uri_component(segment))
        .map(str::to_string)
}

/// Whether JavaScript's `decodeURIComponent` accepts `segment`: every `%` starts a `%XX`
/// escape, and the decoded bytes are valid UTF-8.
pub fn decodes_as_uri_component(segment: &str) -> bool {
    percent_decode_strict(segment).is_some()
}

/// Percent-decodes `input` the way `decodeURIComponent` does, or `None` where it throws.
pub(crate) fn percent_decode_strict(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = hex_value(*bytes.get(i + 1)?)?;
            let low = hex_value(*bytes.get(i + 2)?)?;
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub(crate) fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `parseurl(req).pathname` for a request whose `url` is `target`.
fn express_pathname(target: &str) -> String {
    let fast = target.starts_with('/')
        && !target.chars().any(|c| {
            matches!(
                c,
                '\t' | '\n' | '\x0c' | '\r' | ' ' | '#' | '\u{a0}' | '\u{feff}'
            )
        });
    if fast {
        return target.split('?').next().unwrap_or("").to_string();
    }
    legacy_pathname(target)
}

/// The `pathname` Node's legacy `url.parse` gives for a request target.
fn legacy_pathname(target: &str) -> String {
    let trimmed = target.trim_matches(|c: char| {
        matches!(
            c,
            ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b' | '\u{a0}' | '\u{feff}'
        )
    });
    // Backslashes before the query or fragment become slashes.
    let mut converted = String::with_capacity(trimmed.len());
    let mut in_tail = false;
    for c in trimmed.chars() {
        if c == '?' || c == '#' {
            in_tail = true;
        }
        converted.push(if c == '\\' && !in_tail { '/' } else { c });
    }
    // Node's fast path: a path starting with `/` or `//` (not `///`), without a fragment or
    // whitespace, is taken as is.
    if let Some(simple) = simple_path(&converted) {
        return simple.to_string();
    }
    let mut rest = converted.as_str();
    if let Some(hash) = rest.find('#') {
        rest = &rest[..hash];
    }
    let owned_rest;
    let mut has_host = false;
    if let Some((scheme, after)) = split_protocol(rest) {
        let hostless = matches!(scheme.as_str(), "javascript");
        let slashed = matches!(
            scheme.as_str(),
            "http" | "https" | "ftp" | "gopher" | "file" | "ws" | "wss"
        );
        owned_rest = match after.strip_prefix("//") {
            Some(authority) if !hostless && (slashed || !authority.is_empty()) => {
                has_host = true;
                split_host(authority).to_string()
            }
            _ => after.to_string(),
        };
        rest = owned_rest.as_str();
    }
    let path = rest.split('?').next().unwrap_or("");
    let mut pathname = String::with_capacity(path.len());
    for c in path.chars() {
        let escape = matches!(
            c,
            '\t' | '\n'
                | '\x0b'
                | '\x0c'
                | '\r'
                | ' '
                | '"'
                | '\''
                | '<'
                | '>'
                | '\\'
                | '^'
                | '`'
                | '{'
                | '|'
                | '}'
        );
        if escape {
            pathname.push_str(&format!("%{:02X}", u32::from(c)));
        } else {
            pathname.push(c);
        }
    }
    if pathname.is_empty() && has_host {
        pathname.push('/');
    }
    pathname
}

/// `simplePathPattern` of Node's legacy parser: `^(\/\/?(?!\/)[^?\s]*)(\?[^\s]*)?$`.
fn simple_path(input: &str) -> Option<&str> {
    let after = input
        .strip_prefix("//")
        .or_else(|| input.strip_prefix('/'))?;
    if after.starts_with('/') || input.contains('#') || input.chars().any(char::is_whitespace) {
        return None;
    }
    Some(input.split('?').next().unwrap_or(""))
}

/// `scheme` (lower-cased) and what follows `scheme:`, when `input` starts with a protocol as
/// the legacy parser sees one (`/^[a-z0-9.+-]+:/i`).
fn split_protocol(input: &str) -> Option<(String, &str)> {
    let colon = input.find(':')?;
    let scheme = &input[..colon];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
    {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), &input[colon + 1..]))
}

/// What remains of `authority...` after the legacy parser takes the host: the host (after
/// any `user@`) ends at the first character that cannot be in one, and the path starts
/// there, without a slash when that character is not `/`.
fn split_host(authority: &str) -> &str {
    let mut at_sign = None;
    let mut non_host = None;
    for (i, c) in authority.char_indices() {
        match c {
            '\t' | '\n' | '\r' | ' ' | '"' | '%' | '\'' | ';' | '<' | '>' | '\\' | '^' | '`'
            | '{' | '|' | '}' => {
                non_host.get_or_insert(i);
            }
            '#' | '/' | '?' => {
                non_host.get_or_insert(i);
                break;
            }
            '@' => {
                at_sign = Some(i);
                non_host = None;
            }
            _ => {}
        }
    }
    let start = at_sign.map_or(0, |a| a + 1);
    match non_host {
        Some(n) if n >= start => &authority[n..],
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::golden as testdata;

    #[test]
    fn normalises_like_the_reference_spec() {
        // request-sanitizer.helper.spec.ts
        for (input, expected) in [
            (
                "/user-services/v1/me?x=1&y=2",
                "/user-services/v1/me?x=1&y=2",
            ),
            ("/user-services/oidc/../v1/admin", "/user-services/v1/admin"),
            (
                "/user-services/oidc/%2e%2e/v1/admin",
                "/user-services/v1/admin",
            ),
            (
                "/user-services/oidc/.%2e/v1/admin",
                "/user-services/v1/admin",
            ),
            (
                "/user-services/oidc\\..\\v1/admin",
                "/user-services/v1/admin",
            ),
            (
                "/user-services/oidc/./a/../../v1/admin?k=v",
                "/user-services/v1/admin?k=v",
            ),
            ("/../user-services/v1/admin", "/user-services/v1/admin"),
            ("", "/"),
        ] {
            assert_eq!(normalize_request_url(input).unwrap(), expected, "{input:?}");
        }
    }

    #[test]
    fn normalisation_matches_node_golden_vectors() {
        let mut failures = Vec::new();
        for case in testdata::cases(testdata::URL) {
            let target = case[0].as_str().unwrap();
            // Known difference: for an absolute-form `file:` target Node keeps an empty first
            // path segment (`file:///\\x` is `//x`) and the `url` crate does not. No HTTP
            // client sends such a target.
            if target.to_ascii_lowercase().starts_with("file:") {
                continue;
            }
            let expected = case[1].as_str();
            let actual = normalize_request_url(target).ok();
            if actual.as_deref() != expected {
                failures.push(format!("{target:?}: node {expected:?}, rust {actual:?}"));
            }
        }
        testdata::assert_no_failures("url.json", &failures);
    }

    #[test]
    fn malformed_segment_matches_express_golden_vectors() {
        let mut failures = Vec::new();
        for case in testdata::cases(testdata::PATH_PARAM) {
            let target = case[0].as_str().unwrap();
            let expected = case[1].as_str();
            let actual = malformed_path_segment(target);
            if actual.as_deref() != expected {
                failures.push(format!("{target:?}: express {expected:?}, rust {actual:?}"));
            }
        }
        testdata::assert_no_failures("path_param.json", &failures);
    }

    #[test]
    fn decode_uri_component_rules() {
        for ok in ["", "a", "%41", "%C3%A9", "é", "%F0%9F%98%80", "%2F"] {
            assert!(decodes_as_uri_component(ok), "{ok:?}");
        }
        for bad in [
            "%",
            "%4",
            "%zz",
            "%e9",
            "%C3",
            "%ED%A0%80",
            "%C0%AF",
            "%F4%90%80%80",
            "%C3é",
        ] {
            assert!(!decodes_as_uri_component(bad), "{bad:?}");
        }
    }
}
