//! Micro-benchmarks of the per-request hot paths, one group per pipeline stage, called the
//! way `proxy::run` and `proxy::forward` call them (see `crates/rust-gateway/src/proxy/`).
//!
//! Run with `cargo bench -p api-gateway --bench hot_paths`; `benches/README.md` explains how
//! to read the groups together with the `pipeline` benchmark.

mod common;

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use api_gateway::openapi::build_service_document;
use api_gateway::rate_limit::{TrustProxy, has_status_limits, normalize_ip};
use api_gateway::request::{
    BodyLength, ProxyRequest, RequestClass, classify, forwarded_query, is_supported_method,
    kebab_case, malformed_path_segment, normalize_request_url, response_headers,
    strip_service_prefix, upstream_framing, upstream_request_headers, upstream_target,
};
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, HeaderName, HeaderValue, Method};

use common::*;

/// REQ-14: the Route lookup is a linear scan of the method's Routes in document order, each
/// tried with its compiled pattern. `GET` has 142 Routes in the benchmark document.
fn route_lookup(c: &mut Criterion) {
    let store = store();
    let mut group = c.benchmark_group("route_lookup");
    for (name, method, url) in [
        ("get_first", "GET", "/v1/accounts"),
        ("get_middle_orders_id", "GET", "/v1/orders/42"),
        (
            "get_middle_with_query",
            "GET",
            "/v1/orders/42?expand=items&page=2",
        ),
        ("get_last_health", "GET", "/health"),
        ("get_no_match", "GET", "/v1/unknown/42"),
        ("post_middle_orders", "POST", "/v1/orders"),
        (
            "get_percent_encoded_param",
            "GET",
            "/v1/orders/%E2%9C%93%20x",
        ),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| store.route(PREFIX, black_box(method), black_box(url)))
        });
    }
    group.finish();

    // Not per request: the cost of (re)building the Route table when a document changes
    // (startup and the 60-second refresh, OAS-01 to OAS-05).
    let document: serde_json::Value = serde_json::from_str(DOCUMENT).unwrap();
    let security = security();
    c.bench_function("document_build/282_operations", |b| {
        b.iter(|| {
            build_service_document(
                "http://svc:5000/openapi.json",
                black_box(&document),
                &security,
            )
        })
    });
}

/// REQ-02 and REQ-26: URL normalisation (WHATWG URL resolution) and Express's check for
/// malformed percent-encoding, which runs first in `server::node_compat`.
fn url(c: &mut Criterion) {
    let mut group = c.benchmark_group("url");
    for (name, target) in [
        ("plain", "/svc/v1/orders/42"),
        (
            "with_query",
            "/svc/v1/orders/42?expand=items&page=2&sort=asc",
        ),
        ("dot_segments", "/svc/v1/./orders/../orders/42"),
        (
            "percent_encoded",
            "/svc/v1/orders/%E2%9C%93%20x?q=caf%C3%A9",
        ),
    ] {
        group.bench_function(format!("normalize/{name}"), |b| {
            b.iter(|| normalize_request_url(black_box(target)))
        });
    }
    group.bench_function("malformed_check/plain", |b| {
        b.iter(|| malformed_path_segment(black_box("/svc/v1/orders/42?page=2")))
    });
    group.bench_function("malformed_check/percent_encoded", |b| {
        b.iter(|| malformed_path_segment(black_box("/svc/v1/orders/%E2%9C%93%20x")))
    });
    group.finish();
}

/// REQ-04 to REQ-07, REQ-09 to REQ-12: request class, Upstream Service selection among
/// eight services, and removal of the Service Prefix.
fn service_selection(c: &mut Criterion) {
    let selector = selector();
    let has_document = |prefix: &str| prefix == PREFIX;
    let bypass = ["svc/public", "media/cdn"];
    let mut group = c.benchmark_group("service_selection");
    group.bench_function("classify", |b| {
        b.iter(|| classify(black_box(false), black_box("/svc/v1/orders/42"), &bypass))
    });
    group.bench_function("select/service_prefix_4th_of_8", |b| {
        b.iter(|| selector.select(black_box("/svc/v1/orders/42"), has_document))
    });
    group.bench_function("select/direct_prefix", |b| {
        b.iter(|| selector.select(black_box("/oauth/token"), has_document))
    });
    group.bench_function("select/no_match", |b| {
        b.iter(|| selector.select(black_box("/unknown/v1/orders/42"), has_document))
    });
    group.bench_function("strip_service_prefix", |b| {
        b.iter(|| strip_service_prefix(black_box(PREFIX), black_box("/svc/v1/orders/42")))
    });
    group.bench_function("is_supported_method", |b| {
        b.iter(|| is_supported_method(black_box(&Method::GET)))
    });
    group.finish();
}

/// FWD-01 to FWD-05: the upstream request target (Service Prefix removed, URL resolved
/// again, trailing slash) and the query as undici re-serialises it.
fn upstream_rewrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("upstream_rewrite");
    for (name, url, class) in [
        ("routed", "/svc/v1/orders/42", RequestClass::Routed),
        (
            "routed_with_query",
            "/svc/v1/orders/42?expand=items&page=2&sort=asc",
            RequestClass::Routed,
        ),
        ("static", "/svc/statics/app.css", RequestClass::Static),
    ] {
        group.bench_function(format!("target/{name}"), |b| {
            b.iter(|| upstream_target(black_box(PREFIX), black_box(url), class))
        });
    }
    group.bench_function("forwarded_query", |b| {
        b.iter(|| forwarded_query(black_box("expand=items&page=2&sort=asc&tag=a&tag=b")))
    });
    group.finish();
}

/// REQ-03, FWD-06 to FWD-15: the header rules on the way up and down.
fn header_rules(c: &mut Criterion) {
    let headers = client_headers();
    let lines = client_header_lines();
    let middleware = vec![("auth-user-id".to_string(), "user-42".to_string())];
    let response = upstream_response_headers();
    let mut group = c.benchmark_group("headers");
    // REQ-03 as `proxy::run` does it: filter the lines, then rebuild the map.
    group.bench_function("exclude_and_rebuild_12_lines", |b| {
        b.iter(|| {
            let lines: Vec<(HeaderName, HeaderValue)> = black_box(&headers)
                .iter()
                .filter(|(name, _)| {
                    !EXCLUDE_HEADERS
                        .iter()
                        .any(|excluded| name.as_str().eq_ignore_ascii_case(excluded))
                })
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            let mut map = HeaderMap::with_capacity(lines.len());
            for (name, value) in &lines {
                map.append(name.clone(), value.clone());
            }
            (lines, map)
        })
    });
    group.bench_function("upstream_request_headers_12_lines", |b| {
        b.iter(|| {
            upstream_request_headers(
                black_box(&lines[..11]),
                black_box("172.18.0.5"),
                &Method::GET,
                black_box(&middleware),
            )
        })
    });
    group.bench_function("upstream_framing", |b| {
        b.iter(|| {
            upstream_framing(
                black_box(&Method::POST),
                Some(4096),
                BodyLength::Known(4096),
            )
        })
    });
    group.bench_function("response_headers_8_lines", |b| {
        b.iter(|| response_headers(black_box(&response).iter()))
    });
    group.finish();
}

/// REQ-21, REQ-22: middleware headers set with camel-case names and sent kebab-cased.
fn kebab(c: &mut Criterion) {
    let mut group = c.benchmark_group("kebab");
    group.bench_function("kebab_case/authUserId", |b| {
        b.iter(|| kebab_case(black_box("authUserId")))
    });
    group.bench_function("kebab_case/already_kebab", |b| {
        b.iter(|| kebab_case(black_box("auth-user-id")))
    });
    group.bench_function("proxy_request/add_one_then_kebab_headers", |b| {
        b.iter(|| {
            let mut request = ProxyRequest::new();
            request.add_header("authUserId", black_box("user-42"));
            request.kebab_headers()
        })
    });
    group.bench_function("proxy_request/add_three_then_kebab_headers", |b| {
        b.iter(|| {
            let mut request = ProxyRequest::new();
            request.add_headers([
                ("authUserId", "user-42"),
                ("permissionInAnyGuard", "orders:read"),
                ("xTenantId", "tenant-7"),
            ]);
            request.kebab_headers()
        })
    });
    group.finish();
}

/// RL-06 to RL-08: the client address from the peer and `X-Forwarded-For` under the trusted
/// proxies, normalised; this is the in-memory part of every rate-limit check.
fn client_address(c: &mut Criterion) {
    let sample = limiter(sample_trust());
    let trust_none = limiter(TrustProxy::None);
    let direct = HeaderMap::new();
    let mut forwarded = HeaderMap::new();
    forwarded.insert(
        "x-forwarded-for",
        HeaderValue::from_static("203.0.113.9, 10.0.0.7"),
    );
    let peer = Some(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 5)));
    let peer_v6 = Some(IpAddr::V6(Ipv6Addr::new(
        0x2001, 0xdb8, 0x85a3, 0, 0, 0x8a2e, 0x370, 0x7334,
    )));

    let mut group = c.benchmark_group("client_address");
    group.bench_function("request_state/no_trust_v4", |b| {
        b.iter(|| trust_none.request_state(black_box(peer), black_box(&direct)))
    });
    group.bench_function("request_state/sample_trust_direct_v4", |b| {
        b.iter(|| sample.request_state(black_box(peer), black_box(&direct)))
    });
    group.bench_function("request_state/sample_trust_two_hops", |b| {
        b.iter(|| sample.request_state(black_box(peer), black_box(&forwarded)))
    });
    group.bench_function("request_state/no_trust_v6", |b| {
        b.iter(|| trust_none.request_state(black_box(peer_v6), black_box(&direct)))
    });
    group.bench_function("normalize_ip/v4", |b| {
        b.iter(|| normalize_ip(black_box("172.18.0.5"), 64))
    });
    group.bench_function("normalize_ip/v4_mapped_v6", |b| {
        b.iter(|| normalize_ip(black_box("::ffff:172.18.0.5"), 64))
    });
    group.bench_function("normalize_ip/v6_subnet_64", |b| {
        b.iter(|| normalize_ip(black_box("2001:db8:85a3::8a2e:370:7334"), 64))
    });
    group.finish();
}

/// RL-09 to RL-19 without Redis: what a rate-limit check computes in memory for one Routed
/// Request. The Bucket keys themselves are built inside `RateLimiter` from private
/// functions (`rate_limit::bucket`), so they are only measured as part of the Redis
/// round trip; see `benches/README.md`.
fn rate_limit_in_memory(c: &mut Criterion) {
    let limiter = limiter(sample_trust());
    let store = store();
    let route = store.route(PREFIX, "POST", "/v1/orders").unwrap().unwrap();
    let headers = client_headers();
    let peer = Some(IpAddr::V4(Ipv4Addr::new(172, 18, 0, 5)));
    let mut group = c.benchmark_group("rate_limit_in_memory");
    group.bench_function("state_and_identity", |b| {
        b.iter(|| {
            let state = limiter.request_state(black_box(peer), black_box(&headers));
            let identity = state.default_identity();
            (has_status_limits(&route.rate_limits), identity)
        })
    });
    group.bench_function("has_status_limits", |b| {
        b.iter(|| has_status_limits(black_box(&route.rate_limits)))
    });
    group.finish();
}

fn configured() -> Criterion {
    Criterion::default().configure_from_args()
}

criterion_group! {
    name = benches;
    config = configured();
    targets = route_lookup, url, service_selection, upstream_rewrite, header_rules, kebab,
        client_address, rate_limit_in_memory
}
criterion_main!(benches);
