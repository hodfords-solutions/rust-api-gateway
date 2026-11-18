//! The whole-segment prefix rule (REQ-06) and Service Prefix removal (FWD-01, REQ-14).

/// Whether `url` belongs to `prefix` on a whole-path-segment basis (REQ-06).
///
/// Matches when the URL equals `prefix` or `/prefix`, or starts with one of them followed by
/// `/`, `?` or `#`. Case-sensitive, so `oauthtoken` does not match `oauth`. Used for Direct
/// Prefixes, Service Prefixes and `bypassRoutePrefixes` (which may themselves start with `/`).
pub fn matches_prefix_segment(url: &str, prefix: &str) -> bool {
    let slashed = format!("/{prefix}");
    [prefix, slashed.as_str()].into_iter().any(|base| {
        url == base
            || url
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with(['/', '?', '#']))
    })
}

/// Removes `/prefix` from `url` only when it is the leading path segment (FWD-01):
/// `/svc` becomes `/`, `/svc/x` becomes `/x`, `/svc?q` becomes `?q`, and `/svcx` or `/x/svc`
/// are returned unchanged.
///
/// The Reference Gateway applies this with the selected service's Service Prefix whichever
/// way the service was selected, both to the URL used for Route lookup (REQ-14) and to the
/// path forwarded upstream; paths reached through a Direct Prefix or as the Default Service
/// fallback therefore stay unchanged unless they happen to start with that Service Prefix.
pub fn strip_service_prefix<'a>(prefix: &str, url: &'a str) -> &'a str {
    let base = format!("/{prefix}");
    if url == base {
        return "/";
    }
    match url.strip_prefix(base.as_str()) {
        Some(rest) if rest.starts_with(['/', '?', '#']) => rest,
        _ => url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // prefix.helper.spec.ts
    #[test]
    fn matches_whole_segments() {
        assert!(matches_prefix_segment("/oauth/authorize", "oauth"));
        assert!(matches_prefix_segment("/oauth", "oauth"));
        assert!(matches_prefix_segment("/oauth/", "oauth"));
        assert!(matches_prefix_segment("/oauth?x=1", "oauth"));
        assert!(matches_prefix_segment("/oauth#frag", "oauth"));
        assert!(!matches_prefix_segment("/oauthtoken/x", "oauth"));
        assert!(!matches_prefix_segment("/oauthtoken", "oauth"));
        assert!(matches_prefix_segment("oauth/authorize", "oauth"));
        assert!(matches_prefix_segment("oauth", "oauth"));
        assert!(!matches_prefix_segment("oauthtoken", "oauth"));
        assert!(!matches_prefix_segment("/user-services/login", "oauth"));
        assert!(matches_prefix_segment(
            "/user-services/login",
            "user-services"
        ));
        assert!(matches_prefix_segment("/user-services", "user-services"));
        assert!(!matches_prefix_segment(
            "/user-services-admin/x",
            "user-services"
        ));
    }

    #[test]
    fn matching_is_case_sensitive_and_accepts_slashed_entries() {
        assert!(!matches_prefix_segment("/OAUTH/x", "oauth"));
        assert!(matches_prefix_segment("/svc/public/x", "/svc/public"));
        assert!(matches_prefix_segment("/svc/public/x", "svc/public"));
        assert!(!matches_prefix_segment("/svc/public/x", "public"));
        assert!(matches_prefix_segment("/api/v2/x", "api"));
    }

    // proxy.service.spec.ts, "removePath — leading-only strip"
    #[test]
    fn strips_only_a_leading_prefix() {
        assert_eq!(
            strip_service_prefix("user-services", "/user-services/login"),
            "/login"
        );
        assert_eq!(
            strip_service_prefix("user-services", "/oauth/authorize"),
            "/oauth/authorize"
        );
        assert_eq!(strip_service_prefix("user-services", "/user-services"), "/");
        assert_eq!(
            strip_service_prefix("user-services", "/oauth/user-services/x"),
            "/oauth/user-services/x"
        );
        assert_eq!(strip_service_prefix("svc", "/svc/"), "/");
        assert_eq!(strip_service_prefix("svc", "/svc?x=1"), "?x=1");
        assert_eq!(strip_service_prefix("svc", "/svcx/a"), "/svcx/a");
        assert_eq!(strip_service_prefix("svc", "/svc//v1"), "//v1");
        assert_eq!(strip_service_prefix("api/v2", "/api/v2/health"), "/health");
    }
}
