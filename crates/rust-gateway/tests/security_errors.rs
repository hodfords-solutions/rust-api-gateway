//! Security tests: what error responses reveal (`docs/security/attack-tests.md`).
//!
//! No 4xx or 5xx body or header names an upstream address, the Redis address, an internal
//! path, a handler's error message or the software behind the gateway, and reflected input
//! stays inside a JSON string.

mod gateway_common;

use std::time::Duration;

use api_gateway::gateway::{Gateway, GatewayOptions};
use api_gateway::handler::{
    BoxError, GatewayRequest, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, async_trait,
};
use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use api_gateway::upstream::UpstreamService;
use gateway_common::{RawResponse, Upstream, options, parse_responses, raw_send, serve};

/// A secret-looking message that must never reach a client.
const SECRET: &str = "db password hunter2 at /srv/app/secrets.env on 10.1.2.3:5432";

/// Fails with [`SECRET`] when the request asks for it.
struct Leaky;

#[async_trait]
impl ProxyMiddleware for Leaky {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        match request.header("x-fail") {
            Some(_) => Err(MiddlewareError::other(SECRET)),
            None => Ok(true),
        }
    }
}

/// Fails with [`SECRET`] for `/svc/static-fail`.
struct LeakyStatic;

#[async_trait]
impl StaticRequestPredicate for LeakyStatic {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        if request.url().starts_with("/svc/static-fail") {
            return Err(SECRET.into());
        }
        Ok(false)
    }
}

/// A port nothing listens on.
fn dead_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address").port()
}

async fn fetch(addr: std::net::SocketAddr, request: &[u8]) -> (RawResponse, String) {
    let raw = raw_send(addr, request, Duration::from_secs(5)).await;
    let response = parse_responses(&raw)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("a response to {:?}", String::from_utf8_lossy(request)));
    (response, String::from_utf8_lossy(&raw).into_owned())
}

fn get(target: &str, headers: &str) -> Vec<u8> {
    format!("GET {target} HTTP/1.1\r\nHost: gw\r\n{headers}Connection: close\r\n\r\n").into_bytes()
}

/// Asserts that `raw` (a whole response) reveals none of `secrets`, nor a server banner.
fn assert_reveals_nothing(label: &str, response: &RawResponse, raw: &str, secrets: &[String]) {
    assert!(response.status >= 400, "{label}: {raw:?}");
    for secret in secrets {
        assert!(
            !raw.contains(secret.as_str()),
            "{label} reveals {secret:?}: {raw:?}"
        );
    }
    for word in [
        "hunter2",
        "secrets.env",
        "panicked",
        "src/",
        "hyper::",
        "reqwest",
        "redis://",
    ] {
        assert!(!raw.contains(word), "{label} reveals {word:?}: {raw:?}");
    }
    assert!(response.header("server").is_none(), "{label}: {raw:?}");
    assert!(
        response.header("x-powered-by").is_none(),
        "{label}: {raw:?}"
    );
    assert!(
        serde_json::from_slice::<serde_json::Value>(&response.body).is_ok(),
        "{label}: the body is JSON: {raw:?}"
    );
}

/// Upstream failures — a service whose host refuses connections, one whose document never
/// loaded, and an upgrade to a dead host — are answered without the upstream's address.
#[tokio::test]
async fn upstream_failures_do_not_reveal_upstream_addresses() {
    let documents = Upstream::start().await;
    let down = dead_port();
    let no_document = dead_port();
    let mut options = options(vec![
        UpstreamService::new(
            "svc",
            format!("http://127.0.0.1:{}/openapi.json", documents.addr.port()),
            format!("http://127.0.0.1:{down}"),
        ),
        UpstreamService::new(
            "nodoc",
            format!("http://127.0.0.1:{no_document}/openapi.json"),
            format!("http://127.0.0.1:{no_document}"),
        ),
    ]);
    options.bypass_route_prefixes = vec!["svc/public".into(), "nodoc/public".into()];
    let served = serve(Gateway::builder(options), true).await;
    let secrets = vec![
        down.to_string(),
        no_document.to_string(),
        documents.addr.port().to_string(),
        "127.0.0.1".to_string(),
        "openapi.json".to_string(),
    ];
    for target in [
        "/svc/v1/users",
        "/svc/public/x",
        "/nodoc/v1/users",
        "/nodoc/public/x",
    ] {
        let (response, raw) = fetch(served.addr, &get(target, "")).await;
        assert_reveals_nothing(target, &response, &raw, &secrets);
    }
    // The upgrade path closes the connection with no response at all (WS-16).
    let upgrade = b"GET /svc/ws HTTP/1.1\r\nHost: gw\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
    let raw = raw_send(served.addr, upgrade, Duration::from_secs(3)).await;
    assert!(raw.is_empty(), "{:?}", String::from_utf8_lossy(&raw));
}

/// Handler failures (a middleware or static predicate returning an error that is not an HTTP
/// error) answer the generic 500, never the handler's message.
#[tokio::test]
async fn handler_failures_do_not_reveal_their_messages() {
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")]))
            .proxy_middleware(Leaky)
            .static_request(LeakyStatic),
        true,
    )
    .await;
    let secrets = vec![SECRET.to_string(), upstream.addr.port().to_string()];
    for (label, request) in [
        ("middleware", get("/svc/v1/users", "X-Fail: 1\r\n")),
        ("static predicate", get("/svc/static-fail", "")),
        ("invalid target", get("//[bad/x", "")),
    ] {
        let (response, raw) = fetch(served.addr, &request).await;
        assert_eq!(response.status, 500, "{label}: {raw:?}");
        assert_eq!(
            response.text(),
            r#"{"statusCode":500,"message":"Internal server error"}"#,
            "{label}"
        );
        assert_reveals_nothing(label, &response, &raw, &secrets);
    }
}

/// A rate limiter whose Redis cannot be reached answers the generic 500 (Deviation D10),
/// without the Redis address.
#[tokio::test]
async fn redis_failures_do_not_reveal_the_redis_address() {
    let upstream = Upstream::start().await;
    let redis_port = dead_port();
    let mut options: GatewayOptions = options(vec![upstream.service("svc")]);
    options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(1000, 60.0),
        BucketLimit::new(1000, 60.0),
    );
    options.redis = RedisOptions::new("127.0.0.1", redis_port);
    let served = serve(Gateway::builder(options), true).await;
    let (response, raw) = fetch(served.addr, &get("/svc/v1/users", "")).await;
    assert_eq!(response.status, 500, "{raw:?}");
    assert_reveals_nothing(
        "redis",
        &response,
        &raw,
        &[redis_port.to_string(), "127.0.0.1".to_string()],
    );
}

/// Input reflected in an error (the undecodable segment of REQ-26, the path of a 404) stays
/// inside a JSON string of an `application/json` response: quotes are escaped and `<`, `>`
/// arrive percent-encoded, so the body cannot be read as HTML or break out of the JSON.
#[tokio::test]
async fn reflected_input_stays_inside_json() {
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])),
        true,
    )
    .await;
    for target in [
        "/svc/%zz<script>alert(1)</script>",
        "/svc/%zz\"}]),{\"statusCode\":200,\"x\":\"",
        "/svc/%zz'><img%20src=x%20onerror=alert(1)>",
    ] {
        let (response, raw) = fetch(served.addr, &get(target, "")).await;
        assert_eq!(response.status, 400, "{target}: {raw:?}");
        assert_eq!(
            response.header("content-type"),
            Some("application/json; charset=utf-8"),
            "{target}"
        );
        let body: serde_json::Value =
            serde_json::from_slice(&response.body).expect("the body is JSON");
        assert_eq!(body["statusCode"], 400, "{target}: {body}");
        assert!(!response.text().contains('<'), "{target}: {raw:?}");
        assert_reveals_nothing(target, &response, &raw, &[]);
    }
    let (response, raw) = fetch(
        served.addr,
        b"GET * HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(response.status, 404);
    assert_reveals_nothing("asterisk", &response, &raw, &[]);
}
