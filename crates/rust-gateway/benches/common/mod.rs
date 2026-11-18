//! Inputs shared by the criterion benchmarks: the benchmark Upstream Service's OpenAPI
//! document (`benches/fixtures/bench-service.json` at the repository root, the same file
//! the end-to-end load test serves), a gateway-like configuration, and the header sets of a
//! typical browser request and a typical NestJS response.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use api_gateway::openapi::{DocumentStore, SecurityKeys};
use api_gateway::rate_limit::{
    BucketLimit, RateLimitOptions, RateLimiter, RedisOptions, TrustProxy,
};
use api_gateway::request::ServiceSelector;
use api_gateway::upstream::UpstreamService;
use http::{HeaderMap, HeaderName, HeaderValue};

/// The benchmark service's OpenAPI document (282 operations, 142 of them `GET`).
pub const DOCUMENT: &str = include_str!("../../../../benches/fixtures/bench-service.json");

/// The Service Prefix the benchmark service is mounted under.
pub const PREFIX: &str = "svc";

/// The sample's excluded headers (EX-04).
pub const EXCLUDE_HEADERS: [&str; 2] = ["auth-user-id", "permission-in-any-guard"];

/// A token of the size the end-to-end benchmark sends (HS256, `sub` and `exp`).
pub const BEARER: &str = "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiJ1c2VyLTQyIiwiaWF0IjoxNzk1MDAwMDAwLCJleHAiOjE3OTUwODY0MDB9.5vV6wQ0a8Wm5yQ0bX2Zt1p0bC1u8m0cQe8sKk4pYwqA";

/// Eight Upstream Services, the benchmark service in the middle, two with Direct Prefixes:
/// a mid-sized deployment.
pub fn services() -> Vec<UpstreamService> {
    let names = [
        "auth", "users", "catalog", "svc", "billing", "search", "media", "notify",
    ];
    names
        .iter()
        .map(|name| {
            let service = UpstreamService::new(
                *name,
                format!("http://{name}:5000/openapi.json"),
                format!("http://{name}:5000"),
            );
            match *name {
                "auth" => service.with_direct_prefixes(["oauth", "oidc"]),
                "media" => service.with_direct_prefixes(["images"]),
                _ => service,
            }
        })
        .collect()
}

pub fn selector() -> ServiceSelector {
    ServiceSelector::new(
        services()
            .into_iter()
            .map(|s| (s.prefix.clone(), s.direct_prefixes.clone())),
    )
}

pub fn security() -> SecurityKeys {
    SecurityKeys {
        bearer: vec!["auth-user-id".into()],
        api_key: vec!["x-api-key".into()],
    }
}

/// A store holding the benchmark document for `svc`, loaded the way the loader applies a
/// fetched document.
pub fn store() -> DocumentStore {
    let store = DocumentStore::new(services(), security());
    let document: serde_json::Value = serde_json::from_str(DOCUMENT).expect("fixture is JSON");
    store
        .apply(PREFIX, "http://svc:5000/openapi.json", document)
        .expect("fixture builds");
    store
}

/// The sample's rate-limit options (EX-04) with the trusted proxies of the sample.
pub fn rate_limit_options(trust: TrustProxy) -> RateLimitOptions {
    let mut options =
        RateLimitOptions::new(true, BucketLimit::new(60, 60.0), BucketLimit::new(60, 60.0));
    options.anonymous_ip_limit = Some(60);
    options.anonymous_ip_ttl = Some(60.0);
    options.trust_proxy = trust;
    options
}

/// A limiter whose Redis connection is never opened (only its in-memory part is used).
pub fn limiter(trust: TrustProxy) -> RateLimiter {
    RateLimiter::new(
        &rate_limit_options(trust),
        &RedisOptions::new("127.0.0.1", 1),
    )
    .expect("valid options")
}

pub fn sample_trust() -> TrustProxy {
    TrustProxy::List(vec![
        "loopback".into(),
        "linklocal".into(),
        "uniquelocal".into(),
    ])
}

pub fn peer() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 5)), 51234)
}

fn line(name: &'static str, value: &str) -> (HeaderName, HeaderValue) {
    (
        HeaderName::from_static(name),
        HeaderValue::from_str(value).expect("valid header value"),
    )
}

/// The header lines of a browser-like API request with a bearer token (12 lines).
pub fn client_header_lines() -> Vec<(HeaderName, HeaderValue)> {
    vec![
        line("host", "api.example.com"),
        line("connection", "keep-alive"),
        line("accept", "application/json, text/plain, */*"),
        line("accept-encoding", "gzip, deflate, br"),
        line("accept-language", "en-GB,en;q=0.9,vi;q=0.8"),
        line("authorization", BEARER),
        line("origin", "https://app.example.com"),
        line("referer", "https://app.example.com/orders/42"),
        line(
            "user-agent",
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        ),
        line("x-request-id", "5f0c6c1e-8c1a-4a53-9a0b-3f1a2a7d9e11"),
        line("sec-fetch-mode", "cors"),
        // Sent by a client trying to impersonate a user; the gateway must drop it (REQ-03).
        line("auth-user-id", "admin"),
    ]
}

pub fn client_headers() -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in client_header_lines() {
        map.append(name, value);
    }
    map
}

/// The response headers of a NestJS service answering a small JSON body (8 lines).
pub fn upstream_response_headers() -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in [
        line("x-powered-by", "Express"),
        line("access-control-allow-origin", "*"),
        line("content-type", "application/json; charset=utf-8"),
        line("content-length", "96"),
        line("etag", "W/\"60-3bW6vDqFQ0E4fQx0p7h1vTt4i8Y\""),
        line("date", "Wed, 18 Nov 2026 08:00:00 GMT"),
        line("connection", "keep-alive"),
        line("keep-alive", "timeout=5"),
    ] {
        map.append(name, value);
    }
    map
}
