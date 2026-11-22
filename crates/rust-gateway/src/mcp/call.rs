//! The upstream request of a tool call (MCP-17, MCP-18): the reference's
//! `extractRequestParts` and `buildRequestOptions`, as pure functions.
//!
//! # Path arguments (Deviation D7)
//!
//! The reference substitutes `path_<x>` values into the OpenAPI path raw, so `../x`, `a/b`,
//! `x?y=1` or `x#y` change which upstream operation is called. The port encodes every value
//! before substitution and checks the result:
//!
//! 1. The value is converted with JavaScript's `String()` (as in the reference), then every
//!    UTF-8 byte except `A-Z a-z 0-9 - _ . ! ~ * ' ( )` is percent-encoded with upper-case
//!    hex digits. These are the characters `encodeURIComponent` leaves alone. So `/`, `\`,
//!    `?`, `#`, `%`, `{`, `}`, `$`, spaces, control characters and every non-ASCII character
//!    are encoded: a value cannot add a segment, start a query or fragment, create or fill a
//!    placeholder, or act as a `String.prototype.replace` pattern (the reference turns `$&`
//!    into the placeholder text).
//! 2. Encoding cannot neutralise a dot segment: URL parsers treat `%2E` like `.`. So after
//!    every substitution, a path segment that contains (part of) an argument must not be
//!    empty, `.` or `..`, counting `%2E` and `%2e` as a dot. Otherwise the call fails with
//!    the error result `Error executing <tool>: Invalid path argument path_<x>` and nothing
//!    is sent. An empty segment is refused too, because servers that merge slashes would
//!    send `/users//` to `/users/`, another operation.
//!
//! The rest follows the reference: only the first occurrence of `{x}` is replaced, a
//! placeholder with no argument stays in the path as `{x}`, and an argument for an unknown
//! placeholder is ignored.

use serde_json::{Map, Value};

use super::js::{self, JsMap};
use crate::request::ProxyRequest;
use crate::route::RouteMethod;

/// The request a tool call sends upstream: method, target and headers as the reference
/// hands them to undici.
///
/// The HTTP client adds the rest, as undici does for the reference: `host` first, then
/// `connection: keep-alive` (`connection: close` for `HEAD`), then these headers in order,
/// then `content-length` when there is a body, and `content-length: 0` for `POST`, `PUT` and
/// `PATCH` without one (MCP-18, observed in `mcp::mcp_17_upstream_request`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamCall {
    /// The Route's method.
    pub method: RouteMethod,
    /// Path and query, for example `/users/42/?status=active`. The Service Prefix is not
    /// part of it (MCP-20).
    pub path_and_query: String,
    /// Header names and values in order. Names keep the case they were given; two names
    /// that differ only in case are two headers, as in the reference.
    pub headers: Vec<(String, String)>,
    /// The JSON body, for `POST`, `PUT` and `PATCH` with a truthy `body` argument.
    pub body: Option<String>,
}

impl UpstreamCall {
    /// The upper-case method name, as sent.
    pub fn method_name(&self) -> String {
        self.method.as_str().to_uppercase()
    }
}

/// A path argument that would produce an empty or dot segment (Deviation D7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Invalid path argument path_{name}")]
pub struct PathArgumentError {
    /// The parameter name, without the `path_` prefix.
    pub name: String,
}

/// Builds the upstream request of a tool call (MCP-17, MCP-18).
///
/// - `path` is the OpenAPI path (`/users/{id}`), `method` the Route's method.
/// - `arguments` are the tool arguments: `path_<x>`, `query_<x>`, `header_<x>` and `body`;
///   other names are ignored. Values are converted with JavaScript's `String()`.
/// - `middleware` holds the headers the proxy middleware added for this call, and
///   `exclude_headers` the configured `excludeHeaders`.
///
/// Headers, in order of increasing precedence: `content-type: application/json`, the
/// `header_<x>` arguments except `exclude_headers` names (compared case-insensitively), then
/// the middleware headers (kebab-cased). Empty values are dropped.
///
/// **Deviation D16:** an argument whose name differs only in letter case from a middleware
/// header is dropped, so the middleware's value is the only line of that name. The reference
/// keeps both (`Authorization: <argument>` and `authorization: <middleware>`), and a Node
/// upstream reads the first `authorization`, the argument's. An argument with exactly the
/// middleware's name was already replaced in place, as in the reference.
pub fn build_upstream_call(
    method: RouteMethod,
    path: &str,
    arguments: &Map<String, Value>,
    middleware: &ProxyRequest,
    exclude_headers: &[String],
) -> Result<UpstreamCall, PathArgumentError> {
    let mut path = TrackedPath::new(path);
    let mut query: JsMap<String> = JsMap::new();
    let mut headers: JsMap<String> = JsMap::new();
    let mut body: Option<&Value> = None;

    for (key, value) in arguments {
        if let Some(name) = key.strip_prefix("path_") {
            path.substitute(name, &encode_path_argument(value));
        } else if let Some(name) = key.strip_prefix("query_") {
            query.insert(name.to_string(), js::string(value));
        } else if let Some(name) = key.strip_prefix("header_") {
            headers.insert(name.to_string(), js::string(value));
        } else if key == "body" {
            body = Some(value);
        }
    }
    path.check()?;
    let mut target = path.text;
    if !target.ends_with('/') {
        target.push('/');
    }

    let excluded: Vec<String> = exclude_headers.iter().map(|h| h.to_lowercase()).collect();
    let middleware_headers = middleware.kebab_headers();
    let owned_elsewhere = |name: &str| {
        middleware_headers
            .iter()
            .any(|(owned, _)| owned != name && owned.eq_ignore_ascii_case(name))
    };
    let mut custom: JsMap<String> = JsMap::new();
    for (name, value) in headers.into_entries() {
        if !excluded.contains(&name.to_lowercase()) && !owned_elsewhere(&name) {
            custom.insert(name, value);
        }
    }
    for (name, value) in middleware_headers {
        custom.insert(name, value);
    }
    let mut request_headers = vec![("content-type".to_string(), "application/json".to_string())];
    for (name, value) in custom.into_entries() {
        if value.is_empty() {
            continue;
        }
        match request_headers.iter_mut().find(|(n, _)| *n == name) {
            Some(existing) => existing.1 = value,
            None => request_headers.push((name, value)),
        }
    }

    let query_string = form_urlencode(&query.into_entries());
    if !query_string.is_empty() {
        target.push('?');
        target.push_str(&query_string);
    }

    let body = body
        .filter(|b| js::truthy(b))
        .filter(|_| {
            matches!(
                method,
                RouteMethod::Post | RouteMethod::Put | RouteMethod::Patch
            )
        })
        .map(js::stringify);

    Ok(UpstreamCall {
        method,
        path_and_query: target,
        headers: request_headers,
        body,
    })
}

/// `String(value)` percent-encoded for a path segment (Deviation D7): every UTF-8 byte except
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )` becomes `%XX`.
pub fn encode_path_argument(value: &Value) -> String {
    let text = js::string(value);
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `new URLSearchParams(pairs).toString()`: the `application/x-www-form-urlencoded`
/// serializer (`*-._`, letters and digits kept, space as `+`, everything else `%XX`).
pub fn form_urlencode(pairs: &[(String, String)]) -> String {
    let encode = |text: &str, out: &mut String| {
        for byte in text.bytes() {
            match byte {
                b' ' => out.push('+'),
                b'*' | b'-' | b'.' | b'_' => out.push(char::from(byte)),
                b if b.is_ascii_alphanumeric() => out.push(char::from(b)),
                b => out.push_str(&format!("%{b:02X}")),
            }
        }
    };
    let mut out = String::new();
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        encode(name, &mut out);
        out.push('=');
        encode(value, &mut out);
    }
    out
}

/// The path being built, with the start of every substituted argument marked so that the
/// segments arguments landed in can be checked.
struct TrackedPath {
    text: String,
    /// (byte offset, parameter name) of each substitution.
    marks: Vec<(usize, String)>,
}

impl TrackedPath {
    fn new(template: &str) -> Self {
        Self {
            text: template.to_string(),
            marks: Vec::new(),
        }
    }

    /// `path.replace("{name}", encoded)`: the first occurrence only. The encoded value
    /// contains no `$`, so no replacement pattern applies.
    fn substitute(&mut self, name: &str, encoded: &str) {
        let placeholder = format!("{{{name}}}");
        let Some(start) = self.text.find(&placeholder) else {
            return;
        };
        let end = start + placeholder.len();
        self.text.replace_range(start..end, encoded);
        for (mark, _) in &mut self.marks {
            if *mark >= end {
                *mark = *mark - placeholder.len() + encoded.len();
            } else if *mark > start {
                *mark = start;
            }
        }
        self.marks.push((start, name.to_string()));
    }

    fn check(&self) -> Result<(), PathArgumentError> {
        for (offset, name) in &self.marks {
            let segment_start = self.text[..*offset].rfind('/').map_or(0, |i| i + 1);
            let segment_end = self.text[*offset..]
                .find('/')
                .map_or(self.text.len(), |i| offset + i);
            let segment = self.text[segment_start..segment_end]
                .replace("%2E", ".")
                .replace("%2e", ".");
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(PathArgumentError { name: name.clone() });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(path: &str, arguments: Value) -> Result<UpstreamCall, PathArgumentError> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments must be an object");
        };
        build_upstream_call(
            RouteMethod::Get,
            path,
            &arguments,
            &ProxyRequest::new(),
            &[],
        )
    }

    fn target(path: &str, arguments: Value) -> String {
        call(path, arguments)
            .map(|c| c.path_and_query)
            .unwrap_or_else(|e| e.to_string())
    }

    // Deviation D7: traversal attempts never reach another path.
    #[test]
    fn d7_path_arguments_cannot_change_the_path() {
        let cases = [
            ("../admin", "/users/..%2Fadmin/"),
            ("a/b", "/users/a%2Fb/"),
            ("a\\b", "/users/a%5Cb/"),
            ("x?admin=1", "/users/x%3Fadmin%3D1/"),
            ("x#y", "/users/x%23y/"),
            ("x$&y", "/users/x%24%26y/"),
            ("{id}", "/users/%7Bid%7D/"),
            ("%2e%2e", "/users/%252e%252e/"),
            ("a b\u{e9}", "/users/a%20b%C3%A9/"),
            ("...", "/users/.../"),
            (".x", "/users/.x/"),
            ("!~*'()", "/users/!~*'()/"),
            ("\u{0}\n", "/users/%00%0A/"),
            (".", "Invalid path argument path_id"),
            ("..", "Invalid path argument path_id"),
            ("", "Invalid path argument path_id"),
        ];
        for (value, expected) in cases {
            assert_eq!(
                target("/users/{id}", json!({ "path_id": value })),
                expected,
                "{value:?}"
            );
        }
    }

    #[test]
    fn d7_dot_segments_are_checked_per_segment() {
        // Two arguments in one segment together make "..".
        assert_eq!(
            target("/x/{a}{b}", json!({ "path_a": ".", "path_b": "." })),
            "Invalid path argument path_a"
        );
        assert_eq!(
            target("/x/{a}{b}", json!({ "path_a": ".", "path_b": "x" })),
            "/x/.x/"
        );
        // An empty argument next to literal text leaves a non-empty segment.
        assert_eq!(
            target("/files/{name}.json", json!({ "path_name": "" })),
            "/files/.json/"
        );
        assert_eq!(
            target("/files/{name}.", json!({ "path_name": "." })),
            "Invalid path argument path_name"
        );
        // A template whose own segments are empty or dots is not the argument's doing.
        assert_eq!(
            target("/a//{id}/./b", json!({ "path_id": "1" })),
            "/a//1/./b/"
        );
        // Non-string values.
        assert_eq!(
            target(
                "/{a}/{b}/{c}",
                json!({ "path_a": 1.0, "path_b": [1, 2], "path_c": null })
            ),
            "/1/1%2C2/null/"
        );
        assert_eq!(
            target("/{a}", json!({ "path_a": [] })),
            "Invalid path argument path_a"
        );
    }

    #[test]
    fn first_occurrence_only_and_unknown_placeholders() {
        assert_eq!(
            target("/a/{id}/b/{id}", json!({ "path_id": "7" })),
            "/a/7/b/{id}/"
        );
        assert_eq!(
            target("/users/{id}", json!({ "path_nope": "x" })),
            "/users/{id}/"
        );
        assert_eq!(target("/health/", json!({})), "/health/");
    }

    #[test]
    fn form_urlencode_matches_url_search_params() {
        let pairs = [("a b".to_string(), "x&y=z/\u{e9}~*-._!".to_string())];
        assert_eq!(form_urlencode(&pairs), "a+b=x%26y%3Dz%2F%C3%A9%7E*-._%21");
        assert_eq!(form_urlencode(&[]), "");
    }
}
