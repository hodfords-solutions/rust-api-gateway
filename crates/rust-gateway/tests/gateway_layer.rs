//! Rust-only tests of REQ-27: the application's layer around every HTTP response of a
//! served gateway (`GatewayBuilder::layer`). The Reference Gateway has no counterpart in the
//! library; its host application gets the same effect from Express middleware such as
//! `app.enableCors()`.

mod gateway_common;
mod websocket_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{
    BoxError, GatewayRequest, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, async_trait,
};
use api_gateway::rate_limit::{BucketLimit, RateLimitOptions, RedisOptions};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use gateway_common::{Upstream, client, options, raw_exchange, serve};
use http::header::{self, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use tokio::io::AsyncWriteExt as _;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use websocket_common::{EchoUpstream, read_head, upgrade_request};

const X_LAYER: HeaderName = HeaderName::from_static("x-layer");

/// Counts the requests a middleware saw; denies those with `x-deny`.
#[derive(Clone, Default)]
struct Counting {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProxyMiddleware for Counting {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(request.header("x-deny").is_none())
    }
}

/// Requests whose URL contains `/static/` are Static Requests.
struct StaticFiles;

#[async_trait]
impl StaticRequestPredicate for StaticFiles {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        Ok(request.url().contains("/static/"))
    }
}

/// Counts the requests the layer saw.
async fn count_requests(
    State(seen): State<Arc<AtomicUsize>>,
    request: Request,
    next: Next,
) -> Response {
    seen.fetch_add(1, Ordering::SeqCst);
    next.run(request).await
}

/// A labelled request (method, path, extra headers) and the status it must get, or 0 for
/// any.
type Case = (
    &'static str,
    Method,
    &'static str,
    Vec<(&'static str, &'static str)>,
    u16,
);

/// REQ-27: the layer sees every response the gateway sends over HTTP, and the header it sets
/// overrides the response's own: proxied Routed, Static and Bypass responses, the gateway's
/// errors (the pipeline's and Express's own checks), the document endpoints, `/mcp` and the
/// application's routes.
#[tokio::test]
async fn req_27_layer_sees_every_http_response() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.documents.enabled = true;
    gateway_options.bypass_route_prefixes = vec!["svc/public".into()];
    #[cfg(feature = "mcp")]
    {
        gateway_options.mcp = Some(api_gateway::mcp::McpOptions::enabled());
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let served = serve(
        Gateway::builder(gateway_options)
            .proxy_middleware(Counting::default())
            .static_request(StaticFiles)
            .routes(Router::new().route("/app", get(|| async { "app" })))
            .layer(SetResponseHeaderLayer::overriding(
                X_LAYER,
                HeaderValue::from_static("seen"),
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&seen),
                count_requests,
            )),
        true,
    )
    .await;

    let mut cases: Vec<Case> = vec![
        ("routed", Method::GET, "/svc/v1/users", vec![], 200),
        // The upstream's own `x-layer` is overridden on the way out.
        (
            "routed, header overridden",
            Method::GET,
            "/svc/v1/users?header=x-layer:upstream",
            vec![],
            200,
        ),
        ("static", Method::GET, "/svc/static/logo.png", vec![], 200),
        ("bypass", Method::GET, "/svc/public/anything", vec![], 200),
        (
            "no Route (405)",
            Method::POST,
            "/svc/v1/limited",
            vec![],
            405,
        ),
        (
            "middleware denies (403)",
            Method::GET,
            "/svc/v1/users",
            vec![("x-deny", "1")],
            403,
        ),
        ("no service (503)", Method::GET, "/nowhere/x", vec![], 503),
        ("malformed path (400)", Method::GET, "/svc/%zz", vec![], 400),
        ("document page", Method::GET, "/documents", vec![], 200),
        (
            "document json",
            Method::GET,
            "/document-json?type=svc",
            vec![],
            200,
        ),
        ("application route", Method::GET, "/app", vec![], 200),
    ];
    if cfg!(feature = "mcp") {
        cases.push(("mcp", Method::POST, "/mcp", vec![], 0));
    }
    for (label, method, path, headers, status) in &cases {
        let mut request = client().request(method.clone(), served.url(path));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        if *status != 0 {
            assert_eq!(response.status().as_u16(), *status, "{label}");
        }
        let values: Vec<_> = response.headers().get_all(&X_LAYER).iter().collect();
        assert_eq!(values, ["seen"], "{label}");
    }
    assert_eq!(seen.load(Ordering::SeqCst), cases.len());

    // The asterisk-form target answers Express's 404, through the layer.
    let (status, headers) = raw_exchange(
        served.addr,
        "OPTIONS * HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    assert!(
        headers.contains(&("x-layer".into(), "seen".into())),
        "{headers:?}"
    );
}

/// REQ-27: a layer can answer a request itself: neither the proxy, nor the middleware chain,
/// nor the rate limiter run. The driving case is a CORS preflight; the CORS headers also
/// reach the gateway's own error responses. Redis is unreachable here, so any request that
/// reached the rate limiter would answer 500 (Deviation D10).
#[tokio::test]
async fn req_27_layer_short_circuits_before_the_pipeline() {
    let upstream = Upstream::start().await;
    let mut gateway_options = options(vec![upstream.service("svc")]);
    gateway_options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(100, 60.0),
        BucketLimit::new(100, 60.0),
    );
    gateway_options.redis = RedisOptions::new("127.0.0.1", 1);
    let middleware = Counting::default();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            origin
                .to_str()
                .is_ok_and(|origin| origin.ends_with(".example.com"))
        }))
        .allow_credentials(true)
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
        .max_age(Duration::from_secs(600))
        .expose_headers([header::CONTENT_DISPOSITION, header::RETRY_AFTER]);
    let served = serve(
        Gateway::builder(gateway_options)
            .proxy_middleware(middleware.clone())
            .layer(cors),
        true,
    )
    .await;

    let preflight = client()
        .request(Method::OPTIONS, served.url("/svc/v1/users"))
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::OK);
    let header = |name: &str| {
        preflight
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    assert_eq!(
        header("access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header("access-control-allow-credentials").as_deref(),
        Some("true")
    );
    assert_eq!(
        header("access-control-allow-methods").as_deref(),
        Some("POST")
    );
    assert_eq!(
        header("access-control-allow-headers").as_deref(),
        Some("authorization")
    );
    assert_eq!(header("access-control-max-age").as_deref(), Some("600"));
    // The short-circuited answer is still framed the way Node frames every response.
    assert!(header("date").is_some());
    assert_eq!(header("connection").as_deref(), Some("keep-alive"));
    assert_eq!(header("keep-alive").as_deref(), Some("timeout=5"));
    assert_eq!(header("content-length").as_deref(), Some("0"));
    assert_eq!(middleware.calls.load(Ordering::SeqCst), 0);

    // A real request reaches the pipeline: the rate limiter fails (D10), and the error
    // response carries the CORS headers too.
    let response = client()
        .get(served.url("/svc/v1/users"))
        .header("origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert_eq!(
        response.headers()["access-control-expose-headers"],
        "content-disposition,retry-after"
    );
    // An origin the policy does not allow gets no CORS headers.
    let other = client()
        .get(served.url("/svc/v1/users"))
        .header("origin", "https://evil.test")
        .send()
        .await
        .unwrap();
    assert!(!other.headers().contains_key("access-control-allow-origin"));
    assert!(upstream.seen().is_empty(), "nothing was forwarded");
}

/// REQ-27: default response headers, set only when the response has none of its own
/// (`Cache-Control`) or always (`X-Content-Type-Options`), on proxied responses and on the
/// gateway's own.
#[tokio::test]
async fn req_27_layer_sets_default_response_headers() {
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")]))
            .layer(SetResponseHeaderLayer::if_not_present(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            ))
            .layer(SetResponseHeaderLayer::if_not_present(
                header::PRAGMA,
                HeaderValue::from_static("no-cache"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            )),
        true,
    )
    .await;
    let get = |path: &str| client().get(served.url(path)).send();
    let plain = get("/svc/v1/users").await.unwrap();
    assert_eq!(plain.headers()["cache-control"], "no-store");
    assert_eq!(plain.headers()["pragma"], "no-cache");
    assert_eq!(plain.headers()["x-content-type-options"], "nosniff");

    let cached = get("/svc/v1/users?header=cache-control:max-age=60")
        .await
        .unwrap();
    let values: Vec<_> = cached.headers().get_all("cache-control").iter().collect();
    assert_eq!(values, ["max-age=60"]);

    let error = get("/svc/v1/nothing").await.unwrap();
    assert_eq!(error.status(), 405);
    assert_eq!(error.headers()["cache-control"], "no-store");
    assert_eq!(error.headers()["x-content-type-options"], "nosniff");
}

/// REQ-27: layers wrap one another in registration order, like `Router::layer`: the last
/// registered sees the request first and the response last.
#[tokio::test]
async fn req_27_layers_apply_in_registration_order() {
    async fn first(request: Request, next: Next) -> Response {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .append("x-order", HeaderValue::from_static("first"));
        response
    }
    async fn second(request: Request, next: Next) -> Response {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .append("x-order", HeaderValue::from_static("second"));
        response
    }
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")]))
            .layer(axum::middleware::from_fn(first))
            .layer(axum::middleware::from_fn(second)),
        true,
    )
    .await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    let order: Vec<_> = response.headers().get_all("x-order").iter().collect();
    assert_eq!(order, ["first", "second"]);
}

/// REQ-27: WebSocket upgrades taken over by `Gateway::serve` never reach the layer; they are
/// proxied exactly as without one. Plain requests on the same gateway do reach it.
#[tokio::test]
async fn req_27_layer_does_not_see_websocket_upgrades() {
    let upstream = EchoUpstream::start().await;
    let service = api_gateway::upstream::UpstreamService::new(
        "svc",
        format!("http://{}/openapi.json", upstream.addr),
        format!("http://{}", upstream.addr),
    );
    let seen = Arc::new(AtomicUsize::new(0));
    let served = serve(
        Gateway::builder(options(vec![service]))
            .layer(SetResponseHeaderLayer::overriding(
                X_LAYER,
                HeaderValue::from_static("seen"),
            ))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&seen),
                count_requests,
            )),
        true,
    )
    .await;
    let mut stream = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    stream
        .write_all(&upgrade_request("/svc/socket", ""))
        .await
        .unwrap();
    let head = String::from_utf8(read_head(&mut stream).await).unwrap();
    assert!(
        head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
        "{head}"
    );
    assert!(!head.to_ascii_lowercase().contains("x-layer"), "{head}");
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    drop(stream);
}

/// REQ-27: a layer that does nothing changes no byte of any response head (apart from the
/// `Date`s): the layer's position adds nothing of its own.
#[tokio::test]
async fn req_27_identity_layer_changes_nothing() {
    let upstream = Upstream::start().await;
    let build = |layered: bool| {
        let mut gateway_options = options(vec![upstream.service("svc")]);
        gateway_options.documents.enabled = true;
        let builder = Gateway::builder(gateway_options)
            .routes(Router::new().route("/app", get(|| async { "app" })));
        if layered {
            builder.layer(tower::layer::util::Identity::new())
        } else {
            builder
        }
    };
    let plain = serve(build(false), true).await;
    let layered = serve(build(true), true).await;
    for target in [
        "GET /svc/v1/users",
        "HEAD /svc/v1/users",
        "POST /svc/v1/limited",
        "GET /svc/%zz",
        "OPTIONS *",
        "GET /nowhere/x",
        "GET /app",
        "GET /document-json?type=svc",
        "GET /documents",
    ] {
        let head = format!("{target} HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n");
        let without_date = |(status, headers): (String, Vec<(String, String)>)| {
            let headers: Vec<_> = headers.into_iter().filter(|(n, _)| n != "date").collect();
            (status, headers)
        };
        assert_eq!(
            without_date(raw_exchange(layered.addr, &head).await),
            without_date(raw_exchange(plain.addr, &head).await),
            "{target}"
        );
    }
}
