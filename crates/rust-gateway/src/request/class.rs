//! Request classes (REQ-09, REQ-12) and the supported methods (REQ-15).

use http::Method;

use super::prefix::matches_prefix_segment;

/// How a request is handled once its Upstream Service is selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// A static predicate claimed it: forwarded with no Route lookup, middleware, rate
    /// limiting or middleware headers, and no trailing slash added (REQ-09, REQ-10).
    Static,
    /// Under a `bypassRoutePrefixes` entry: forwarded like a Static Request but with a
    /// trailing slash (REQ-12, REQ-13).
    Bypass,
    /// Everything else: Route lookup, rate limits, middleware (REQ-14 to REQ-19).
    Routed,
}

/// Classifies a request. `is_static` is whether any registered static predicate returned
/// true; it is checked first (REQ-09). Bypass prefixes are matched against the normalised
/// URL with [`matches_prefix_segment`] (REQ-12).
pub fn classify<S: AsRef<str>>(is_static: bool, url: &str, bypass_prefixes: &[S]) -> RequestClass {
    if is_static {
        RequestClass::Static
    } else if is_bypass_request(url, bypass_prefixes) {
        RequestClass::Bypass
    } else {
        RequestClass::Routed
    }
}

/// Whether the normalised `url` matches any `bypassRoutePrefixes` entry (REQ-12). The entry
/// is compared with the full URL, so it includes the Service Prefix when there is one.
pub fn is_bypass_request<S: AsRef<str>>(url: &str, bypass_prefixes: &[S]) -> bool {
    bypass_prefixes
        .iter()
        .any(|prefix| matches_prefix_segment(url, prefix.as_ref()))
}

/// The methods a Route can have: the keys of the Reference Gateway's Route table.
pub const SUPPORTED_METHODS: [&str; 8] = [
    "GET", "POST", "DELETE", "PATCH", "PUT", "OPTIONS", "HEAD", "SEARCH",
];

/// Whether a Routed Request's method can have a Route. Deviation D4 (REQ-15): any other
/// method answers 405, whatever its letter case; the Reference Gateway answers 500 (or a
/// bare 400 from Node's parser for tokens it does not know).
pub fn is_supported_method(method: &Method) -> bool {
    SUPPORTED_METHODS.contains(&method.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_is_checked_before_bypass() {
        let bypass = ["svc/public"];
        assert_eq!(
            classify(true, "/svc/public/x", &bypass),
            RequestClass::Static
        );
        assert_eq!(
            classify(false, "/svc/public/x", &bypass),
            RequestClass::Bypass
        );
        assert_eq!(
            classify(false, "/svc/publicity", &bypass),
            RequestClass::Routed
        );
    }

    // proxy.service.spec.ts, "request sanitization": bypass is decided on the normalised URL
    // and on whole segments.
    #[test]
    fn bypass_uses_whole_segments_and_includes_the_service_prefix() {
        let bypass = ["/user-services/oidc"];
        assert!(is_bypass_request(
            "/user-services/oidc/.well-known/openid-configuration",
            &bypass
        ));
        assert!(!is_bypass_request(
            "/user-services/oidc-admin/users",
            &bypass
        ));
        assert!(!is_bypass_request("/user-services/v1/admin", &bypass));
        assert!(!is_bypass_request("/svc/open/x", &["open"]));
        assert!(is_bypass_request("/open/x", &["open"]));
        assert!(!is_bypass_request("/x", &[] as &[&str]));
    }

    #[test]
    fn supported_methods_are_the_route_table_keys() {
        for m in [
            "GET", "POST", "DELETE", "PATCH", "PUT", "OPTIONS", "HEAD", "SEARCH",
        ] {
            assert!(
                is_supported_method(&Method::from_bytes(m.as_bytes()).unwrap()),
                "{m}"
            );
        }
        for m in ["TRACE", "PROPFIND", "QUERY", "get", "Get", "CONNECT"] {
            assert!(
                !is_supported_method(&Method::from_bytes(m.as_bytes()).unwrap()),
                "{m}"
            );
        }
    }
}
