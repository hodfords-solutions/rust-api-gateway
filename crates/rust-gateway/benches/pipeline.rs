//! The CPU work of one Routed Request, as far as the library lets it be measured without I/O.
//!
//! - `pipeline/composed_routed_get`: every pure step `proxy::run` and `proxy::forward` perform
//!   for `GET /svc/v1/orders/42` with a browser-like header set and a bearer token, called in
//!   the pipeline's order (REQ-16): URL normalisation, header exclusion, the request class,
//!   service selection, the Route lookup, the rate-limit state, a middleware's header, the
//!   upstream target and headers, the body framing, and the response headers. It leaves out
//!   what the library does not expose: axum/hyper request handling, `server::node_compat`,
//!   the handler calls through `async_trait`, the Bucket keys, the connection pool and the
//!   body wrappers.
//! - `pipeline/router_rejected_by_middleware`: the real `Gateway::router()` serving one Routed
//!   Request in-process (no socket), up to a middleware that rejects it with 401, then the
//!   error rendering (JSON body, `ETag`). Rate limiting is disabled, because the limiter's
//!   only store is Redis.
//!
//! What is missing for a full Routed Request with I/O stubbed out, and the seams that would
//! allow it (neither exists today):
//! - the upstream: `UpstreamPool` is private and always dials TCP. A builder method taking a
//!   `tower::Service<http::Request<Body>>` (or a connector) per Upstream Service would let a
//!   benchmark answer in memory.
//! - Redis: `RateLimiter` has an in-memory store only under `cfg(test)`. Exposing it (say,
//!   `RateLimiter::in_memory` behind a `testing` feature) would let the router bench keep
//!   rate limiting on.

mod common;

use std::hint::black_box;
use std::sync::Arc;

use axum::body::Body;
use criterion::{Criterion, criterion_group, criterion_main};
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri};
use http_body_util::BodyExt;
use rust_gateway::Gateway;
use rust_gateway::gateway::GatewayOptions;
use rust_gateway::handler::{GatewayRequest, MiddlewareError, ProxyMiddleware, async_trait};
use rust_gateway::openapi::{DocumentFetcher, FetchFuture, FetchedDocument};
use rust_gateway::rate_limit::{RateLimitState, RedisOptions, RouteKey};
use rust_gateway::request::{
    BodyLength, ProxyRequest, classify, node_remote_address, normalize_request_url,
    response_headers, strip_service_prefix, upstream_framing, upstream_request_headers,
    upstream_target,
};
use rust_gateway::route::Route;
use tower::ServiceExt as _;

use common::*;

fn composed(c: &mut Criterion) {
    let store = store();
    let selector = selector();
    let limiter = limiter(sample_trust());
    let incoming = client_headers();
    let peer = peer();
    let upstream_response = upstream_response_headers();
    let bypass: Vec<String> = Vec::new();

    c.bench_function("pipeline/composed_routed_get", |b| {
        b.iter(|| {
            let target = black_box("/svc/v1/orders/42");
            // REQ-02
            let url = normalize_request_url(target).unwrap();
            // REQ-03
            let lines: Vec<(HeaderName, HeaderValue)> = incoming
                .iter()
                .filter(|(name, _)| {
                    !EXCLUDE_HEADERS
                        .iter()
                        .any(|excluded| name.as_str().eq_ignore_ascii_case(excluded))
                })
                .map(|(n, v)| (n.clone(), v.clone()))
                .collect();
            let mut headers = HeaderMap::with_capacity(lines.len());
            for (name, value) in &lines {
                headers.append(name.clone(), value.clone());
            }
            let request = GatewayRequest::new(
                Method::GET,
                Uri::from_static("/svc/v1/orders/42"),
                url.clone(),
                headers,
                Some(peer),
            );
            // REQ-09 (the sample's static predicate), REQ-12
            let is_static =
                request.url().contains("/statics/") || request.url().contains("/images/");
            let class = classify(is_static, &url, &bypass);
            // REQ-04 to REQ-07
            let selected = selector
                .select(&url, |prefix| store.has_document(prefix))
                .into_result()
                .unwrap();
            let prefix = selected.prefix.to_string();
            // REQ-14
            let route = store
                .route(&prefix, "GET", strip_service_prefix(&prefix, &url))
                .unwrap()
                .unwrap();
            // RL-06, RL-07, RL-11
            let state: RateLimitState = limiter.request_state(Some(peer.ip()), request.headers());
            let identity = state.default_identity();
            let key = RouteKey::new(&prefix, "GET", &route.router_path);
            // REQ-17, REQ-21: the middleware's header
            let mut proxy_request = ProxyRequest::new();
            for name in EXCLUDE_HEADERS {
                proxy_request.remove_header(name);
            }
            proxy_request.add_header("authUserId", "user-42");
            let middleware_headers = proxy_request.kebab_headers();
            // FWD-01 to FWD-13
            let target = upstream_target(&prefix, &url, class).unwrap();
            let peer_address = node_remote_address(peer.ip());
            let list =
                upstream_request_headers(&lines, &peer_address, &Method::GET, &middleware_headers)
                    .unwrap();
            let framing = upstream_framing(&Method::GET, None, BodyLength::Empty).unwrap();
            // FWD-15
            let response = response_headers(upstream_response.iter());
            black_box((target, list, framing, response, identity, key.pattern.len()))
        })
    });
}

/// Serves the benchmark document without a network.
struct FixtureFetcher;

impl DocumentFetcher for FixtureFetcher {
    fn fetch<'a>(&'a self, _url: &'a str) -> FetchFuture<'a> {
        Box::pin(async {
            Ok(FetchedDocument {
                status: 200,
                body: DOCUMENT.as_bytes().to_vec(),
            })
        })
    }
}

/// Rejects every request the way the benchmark middleware rejects a bad token.
struct RejectUnauthorized;

#[async_trait]
impl ProxyMiddleware for RejectUnauthorized {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        let _token = request.header("authorization");
        Err(MiddlewareError::json(
            StatusCode::UNAUTHORIZED,
            serde_json::json!({"message": "Unauthorized", "statusCode": 401}),
        ))
    }
}

fn router(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let router = runtime.block_on(async {
        let mut rate_limit = rate_limit_options(sample_trust());
        rate_limit.enabled = false;
        let mut options =
            GatewayOptions::new(services(), rate_limit, RedisOptions::new("127.0.0.1", 1));
        options.exclude_headers = EXCLUDE_HEADERS.iter().map(|s| s.to_string()).collect();
        options.open_api_security_keys = vec!["auth-user-id".into()];
        let gateway = Gateway::builder(options)
            .document_fetcher(FixtureFetcher)
            .proxy_middleware(RejectUnauthorized)
            .build()
            .unwrap();
        gateway.ready().await;
        gateway.router()
    });
    let router = Arc::new(router);
    let header_lines = client_header_lines();
    let request = move || {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri("/svc/v1/orders/42");
        for (name, value) in &header_lines {
            builder = builder.header(name, value);
        }
        builder
            .extension(axum::extract::ConnectInfo(peer()))
            .body(Body::empty())
            .unwrap()
    };
    // Check once that the request really reaches the middleware.
    let status = runtime.block_on(async {
        let response = router.as_ref().clone().oneshot(request()).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], br#"{"message":"Unauthorized","statusCode":401}"#);
        status
    });
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    c.bench_function("pipeline/router_rejected_by_middleware", |b| {
        b.to_async(&runtime).iter(|| {
            let router = router.as_ref().clone();
            let request = request();
            async move {
                let response = router.oneshot(request).await.unwrap();
                response.into_body().collect().await.unwrap().to_bytes()
            }
        })
    });
}

criterion_group!(benches, composed, router);
criterion_main!(benches);
