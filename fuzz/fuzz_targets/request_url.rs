//! Request-target handling: URL normalisation (REQ-02), the malformed-segment check (REQ-26),
//! request classification, Service Prefix removal and the upstream target (FWD-01 to FWD-04),
//! and the Route-lookup path. Properties: no dot segment survives normalisation or reaches the
//! upstream target, and the two `decodeURIComponent` ports agree.
#![no_main]

use api_gateway::request::{
    classify, decodes_as_uri_component, malformed_path_segment, matches_prefix_segment,
    normalize_request_url, strip_service_prefix, upstream_target,
};
use api_gateway::route::uri::{decode_uri_component, path_from_url};
use api_gateway_fuzz::dot_segments;
use libfuzzer_sys::fuzz_target;

// Input: `target [\0 service prefix [\0 bypass prefix...]]` as (lossy) UTF-8.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut fields = text.split('\0');
    let target = fields.next().unwrap_or("");
    let prefix = fields.next().unwrap_or("svc");
    let bypass: Vec<&str> = fields.take(4).collect();
    let _ = malformed_path_segment(target);

    let strict = decodes_as_uri_component(target);
    let decoded = decode_uri_component(target);
    assert_eq!(
        strict,
        decoded.is_ok(),
        "the two decodeURIComponent ports disagree on {target:?}"
    );

    if let Ok(path) = path_from_url(target)
        && path.starts_with('/')
    {
        assert!(
            dot_segments(&path).is_empty(),
            "Route-lookup path {path:?} of {target:?} keeps a dot segment"
        );
    }

    let Ok(normalized) = normalize_request_url(target) else {
        return;
    };
    // An absolute target with an opaque path (`mailto:x`) has no segments to resolve.
    if normalized.starts_with('/') {
        assert!(
            dot_segments(&normalized).is_empty(),
            "normalised {normalized:?} of {target:?} keeps a dot segment"
        );
    }
    let _ = matches_prefix_segment(&normalized, prefix);
    let _ = strip_service_prefix(prefix, &normalized);
    for is_static in [false, true] {
        let class = classify(is_static, &normalized, &bypass);
        let Ok(upstream) = upstream_target(prefix, &normalized, class) else {
            continue;
        };
        if !normalized.starts_with('/') {
            continue;
        }
        assert!(
            upstream.starts_with('/'),
            "upstream target {upstream:?} of {normalized:?} is not origin-form"
        );
        assert!(
            dot_segments(&upstream).is_empty(),
            "upstream target {upstream:?} of {normalized:?} keeps a dot segment"
        );
    }
});
