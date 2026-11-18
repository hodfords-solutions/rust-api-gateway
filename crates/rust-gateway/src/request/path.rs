//! The path and query forwarded upstream (FWD-01 to FWD-04).

use crate::error::GatewayError;

use super::class::RequestClass;
use super::prefix::strip_service_prefix;
use super::query::forwarded_query;
use super::url::normalize_request_url;

/// The upstream path for a request (FWD-01 to FWD-03), without the query.
///
/// `normalized_url` is the output of
/// [`normalize_request_url`](super::normalize_request_url); `service_prefix` is the selected
/// service's Service Prefix. The Service Prefix is removed when it is the leading segment
/// ([`strip_service_prefix`]), the result is resolved again as a URL (so a path left
/// starting with `//` loses its first segment as an authority: `/svc//v1/users` is forwarded
/// as `/users`), and a trailing `/` is appended unless the request is a Static Request.
///
/// A result that is not a valid URL (for example `/svc//[x`) fails with
/// [`GatewayError::UpstreamUnavailable`], as the Reference Gateway's proxy answers 503.
pub fn upstream_path(
    service_prefix: &str,
    normalized_url: &str,
    class: RequestClass,
) -> Result<String, GatewayError> {
    let path = normalized_url.split('?').next().unwrap_or("");
    let stripped = strip_service_prefix(service_prefix, path);
    let mut resolved =
        normalize_request_url(stripped).map_err(|_| GatewayError::UpstreamUnavailable)?;
    let (pathname, search) = match resolved.find('?') {
        Some(at) => {
            let search = resolved.split_off(at);
            (resolved, search)
        }
        None => (resolved, String::new()),
    };
    let mut out = pathname;
    if !out.ends_with('/') && class != RequestClass::Static {
        out.push('/');
    }
    out.push_str(&search);
    Ok(out)
}

/// The full upstream request target: [`upstream_path`] plus the query re-serialised as the
/// Reference Gateway's HTTP client sends it ([`forwarded_query`], FWD-04).
pub fn upstream_target(
    service_prefix: &str,
    normalized_url: &str,
    class: RequestClass,
) -> Result<String, GatewayError> {
    let mut target = upstream_path(service_prefix, normalized_url, class)?;
    let query = normalized_url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let query = forwarded_query(query);
    if !query.is_empty() {
        target.push('?');
        target.push_str(&query);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routed(prefix: &str, url: &str) -> String {
        upstream_target(prefix, url, RequestClass::Routed).unwrap()
    }

    fn static_(prefix: &str, url: &str) -> String {
        upstream_target(prefix, url, RequestClass::Static).unwrap()
    }

    // proxy.service.spec.ts, "rewritePath"
    #[test]
    fn retains_direct_prefix_and_strips_service_prefix() {
        assert_eq!(
            routed("user-services", "/oauth/authorize"),
            "/oauth/authorize/"
        );
        assert_eq!(routed("user-services", "/user-services/login"), "/login/");
    }

    // Observed in forwarding::fwd_01_path_rewrite.
    #[test]
    fn matches_the_conformance_table() {
        assert_eq!(static_("svc", "/svc"), "/");
        assert_eq!(static_("svc", "/svc/"), "/");
        assert_eq!(static_("svc", "/svc/x"), "/x");
        assert_eq!(static_("svc", "/svc/x/"), "/x/");
        assert_eq!(static_("svc", "/svc?x=1"), "/?x=1");
        assert_eq!(static_("svc", "/svc/svc/x"), "/svc/x");
        assert_eq!(static_("default", "/svcx/a"), "/svcx/a");
        assert_eq!(static_("default", "/default/x"), "/x");
        assert_eq!(routed("svc", "/svc/v1/users"), "/v1/users/");
        assert_eq!(routed("svc", "/svc/v1/users/"), "/v1/users/");
        assert_eq!(routed("svc", "/svc/v1/users?x=1"), "/v1/users/?x=1");
        assert_eq!(routed("default", "/health"), "/health/");
        assert_eq!(routed("default", "/default/health"), "/health/");
        assert_eq!(
            upstream_target("svc", "/svc/v1/paths?q=1", RequestClass::Bypass).unwrap(),
            "/v1/paths/?q=1"
        );
    }

    // Observed in pipeline::req_02_url_normalisation.
    #[test]
    fn a_double_slash_after_the_prefix_is_read_as_an_authority() {
        assert_eq!(static_("svc", "/svc//v1/users"), "/users");
        assert_eq!(static_("svc", "/svc//v1"), "/");
        assert_eq!(static_("svc", "/svc/v1//users"), "/v1//users");
        assert!(matches!(
            upstream_target("svc", "/svc//[x", RequestClass::Static),
            Err(GatewayError::UpstreamUnavailable)
        ));
    }

    #[test]
    fn the_query_is_re_serialised() {
        assert_eq!(
            routed("svc", "/svc/v1/users?a=1&b=2&a=3"),
            "/v1/users/?a=1&a=3&b=2"
        );
        assert_eq!(routed("svc", "/svc/v1/users?&&"), "/v1/users/");
        assert_eq!(static_("svc", "/svc/x?q=%zz"), "/x?q=%25zz");
    }
}
