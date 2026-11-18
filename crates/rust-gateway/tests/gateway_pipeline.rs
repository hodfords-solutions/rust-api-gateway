//! Rust-only tests of the served gateway: startup and readiness (CFG-16), application routes
//! (CFG-18), handler order (CFG-19), values attached by middleware (REQ-20), the D4
//! deviations (REQ-08, REQ-15), Deviation D11 (`X-Powered-By`, FWD-15, REQ-23) and replaceable
//! error rendering (REQ-25).

mod gateway_common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use api_gateway::error::GatewayError;
use api_gateway::gateway::Gateway;
use api_gateway::handler::{
    BoxError, GatewayRequest, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, async_trait,
};
use api_gateway::openapi::{DocumentFetcher, FetchFuture, HttpDocumentFetcher};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use axum::Router;
use axum::routing::get;
use gateway_common::{Upstream, client, options, serve};
use http::{Response, StatusCode};
use tokio::sync::Notify;

/// Fetches documents over HTTP once `gate` is opened.
struct GatedFetcher {
    gate: Arc<Notify>,
    inner: HttpDocumentFetcher,
}

impl DocumentFetcher for GatedFetcher {
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
        Box::pin(async move {
            self.gate.notified().await;
            self.inner.fetch(url).await
        })
    }
}

/// CFG-16: the gateway serves before any document has loaded, and its readiness signal
/// resolves once every first load has settled. The server also stops on its shutdown signal.
#[tokio::test]
async fn cfg_16_serving_does_not_wait_for_documents() {
    let upstream = Upstream::start().await;
    let gate = Arc::new(Notify::new());
    let builder =
        Gateway::builder(options(vec![upstream.service("svc")])).document_fetcher(GatedFetcher {
            gate: Arc::clone(&gate),
            inner: HttpDocumentFetcher::new().expect("fetcher"),
        });
    let served = serve(builder, false).await;
    assert!(!served.gateway.is_ready());

    // Serving already: without a document the service cannot be selected (REQ-07).
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .expect("answered before documents loaded");
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"message":"Service Unavailable","statusCode":503}"#
    );
    assert!(!served.gateway.is_ready());

    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), served.gateway.ready())
        .await
        .expect("ready once the first load settled");
    assert!(served.gateway.is_ready());
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let addr = served.addr;
    served.shutdown().await.expect("server stops");
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "the listener is closed after shutdown"
    );
}

/// CFG-18 (EX-06): the application's own routes take precedence over the proxy, even under
/// a Direct Prefix; everything else reaches the proxy.
#[tokio::test]
async fn cfg_18_application_routes_take_precedence() {
    let upstream = Upstream::start().await;
    let service = upstream.service("svc").with_direct_prefixes(["oauth"]);
    let routes = Router::new().route("/oauth/metadata", get(|| async { "from the application" }));
    let served = serve(
        Gateway::builder(options(vec![service])).routes(routes),
        true,
    )
    .await;

    let response = client()
        .get(served.url("/oauth/metadata"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "from the application");
    assert!(upstream.seen().is_empty());

    // Everything else reaches the proxy.
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "upstream /v1/users/");
}

/// Records the order handlers run in.
struct Recorder {
    name: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

#[derive(Debug, Clone)]
struct AttachedUser(String);

#[async_trait]
impl ProxyMiddleware for Recorder {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        self.log.lock().unwrap().push(self.name);
        match request.extensions().get::<AttachedUser>() {
            // REQ-20: a later middleware reads what an earlier one attached.
            Some(AttachedUser(user)) => proxy_request.add_header("seenUser", user.clone()),
            None => {
                request
                    .extensions_mut()
                    .insert(AttachedUser(format!("attached by {}", self.name)));
            }
        }
        Ok(true)
    }
}

/// CFG-19 and REQ-20: middleware run in ascending priority, ties in registration order, and
/// what one attaches to the request the next one reads.
#[tokio::test]
async fn cfg_19_req_20_middleware_order_and_attached_values() {
    let upstream = Upstream::start().await;
    let log = Arc::new(Mutex::new(Vec::new()));
    let recorder = |name| Recorder {
        name,
        log: Arc::clone(&log),
    };
    let builder = Gateway::builder(options(vec![upstream.service("svc")]))
        .proxy_middleware_with_priority(5, recorder("five"))
        .proxy_middleware(recorder("one-a"))
        .proxy_middleware_with_priority(-1, recorder("minus-one"))
        .proxy_middleware(recorder("one-b"));
    let served = serve(builder, true).await;
    let response = client()
        .get(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        *log.lock().unwrap(),
        ["minus-one", "one-a", "one-b", "five"]
    );
    let seen = upstream.seen();
    let user = seen[0]
        .headers
        .iter()
        .find(|(n, _)| n == "seen-user")
        .map(|(_, v)| v.as_str());
    assert_eq!(user, Some("attached by minus-one"));
}

/// Claims every request as a Static Request.
struct AllStatic;

#[async_trait]
impl StaticRequestPredicate for AllStatic {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        Ok(request.header("x-static").is_some())
    }
}

/// REQ-08 (Deviation D4): a Routed Request to a Direct Prefix owner without a loaded
/// document answers 503; a Static Request is still forwarded. The reference answers 500.
#[tokio::test]
async fn req_08_d4_routed_request_without_document_answers_503() {
    let upstream = Upstream::start().await;
    let mut service = upstream.service("own").with_direct_prefixes(["oauth"]);
    service.doc_url = format!("http://{}/missing.json", upstream.addr);
    let served = serve(
        Gateway::builder(options(vec![service])).static_request(AllStatic),
        true,
    )
    .await;
    assert!(served.gateway.documents().loaded_services().is_empty());

    let response = client()
        .get(served.url("/oauth/token"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.headers()["content-type"],
        "application/json; charset=utf-8"
    );
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"message":"Service Unavailable","statusCode":503}"#
    );

    let response = client()
        .get(served.url("/oauth/token"))
        .header("x-static", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "upstream /oauth/token");
}

/// REQ-15 (Deviation D4): a Routed Request whose method cannot have a Route answers 405,
/// whatever its letter case; a Static Request forwards it.
#[tokio::test]
async fn req_15_d4_unsupported_methods_answer_405() {
    let upstream = Upstream::start().await;
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])).static_request(AllStatic),
        true,
    )
    .await;
    for method in [
        "TRACE", "PROPFIND", "MKCOL", "QUERY", "get", "Get", "FOO", "post",
    ] {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let response = client()
            .request(method.clone(), served.url("/svc/v1/users"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 405, "{method}");
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"message":"Method Not Allowed","statusCode":405}"#,
            "{method}"
        );
    }
    assert!(upstream.seen().is_empty());

    let response = client()
        .request(
            reqwest::Method::from_bytes(b"PROPFIND").unwrap(),
            served.url("/svc/v1/users"),
        )
        .header("x-static", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(upstream.seen()[0].method, "PROPFIND");
}

/// REQ-25: the application can replace how gateway errors are rendered.
#[tokio::test]
async fn req_25_error_rendering_can_be_replaced() {
    let upstream = Upstream::start().await;
    let renderer = |error: &GatewayError| {
        let mut response =
            Response::new(format!("custom {}", error.status().as_u16()).into_bytes());
        *response.status_mut() = if matches!(error, GatewayError::MethodNotAllowed) {
            StatusCode::NOT_FOUND
        } else {
            error.status()
        };
        response
    };
    let served = serve(
        Gateway::builder(options(vec![upstream.service("svc")])).error_renderer(renderer),
        true,
    )
    .await;
    let response = client()
        .put(served.url("/svc/v1/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "custom 405");
    let response = client().get(served.url("/nowhere")).send().await.unwrap();
    assert_eq!(response.status(), 503);
    assert_eq!(response.text().await.unwrap(), "custom 503");
}

/// Deviation D11 (FWD-15, REQ-23): by default no response carries `X-Powered-By` (the
/// reference sends `X-Powered-By: Express` on every one); an upstream's own `x-powered-by` is
/// forwarded like any other header. `x_powered_by(true)` restores Express's header first in
/// every kind of response: forwarded, the gateway's own errors, application routes, the
/// document endpoints and a rejected WebSocket upgrade (D1); an upstream's own value then
/// takes its place.
#[tokio::test]
async fn d11_x_powered_by_is_off_by_default_and_can_be_turned_on() {
    let upstream = Upstream::start().await;
    let requests = [
        ("forwarded", "GET /svc/v1/users HTTP/1.1", "HTTP/1.1 200 OK"),
        (
            "gateway error (no Upstream Service)",
            "GET /nowhere HTTP/1.1",
            "HTTP/1.1 503 Service Unavailable",
        ),
        (
            "gateway error (method)",
            "PATCH /svc/v1/users HTTP/1.1",
            "HTTP/1.1 405 Method Not Allowed",
        ),
        ("application route", "GET /app HTTP/1.1", "HTTP/1.1 200 OK"),
        (
            "document endpoint",
            "GET /documents HTTP/1.1",
            "HTTP/1.1 200 OK",
        ),
        (
            "rejected WebSocket upgrade",
            "GET /nowhere/socket HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
            "HTTP/1.1 503 Service Unavailable",
        ),
    ];
    for enabled in [None, Some(false), Some(true)] {
        let mut gateway_options = options(vec![upstream.service("svc")]);
        gateway_options.documents.enabled = true;
        let mut builder = Gateway::builder(gateway_options)
            .routes(Router::new().route("/app", get(|| async { "app" })));
        if let Some(enabled) = enabled {
            builder = builder.x_powered_by(enabled);
        }
        let served = serve(builder, true).await;
        let on = enabled == Some(true);
        for (label, line, status) in requests {
            let (got, headers) = gateway_common::raw_exchange(
                served.addr,
                &format!("{line}\r\nHost: gw\r\nConnection: close\r\n\r\n"),
            )
            .await;
            assert_eq!(got, status, "{label} ({enabled:?})");
            let powered: Vec<&str> = headers
                .iter()
                .filter(|(n, _)| n == "x-powered-by")
                .map(|(_, v)| v.as_str())
                .collect();
            if on {
                assert_eq!(powered, ["Express"], "{label}: {headers:?}");
                assert_eq!(headers[0].0, "x-powered-by", "{label}: first");
            } else {
                assert!(powered.is_empty(), "{label} ({enabled:?}): {headers:?}");
            }
        }

        // The upstream's own `x-powered-by` is forwarded in either setting, first, alone.
        let (status, headers) = gateway_common::raw_exchange(
            served.addr,
            "GET /svc/v1/users?powered=Mock HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        let powered: Vec<&(String, String)> = headers
            .iter()
            .filter(|(n, _)| n == "x-powered-by")
            .collect();
        assert_eq!(powered, [&("x-powered-by".to_string(), "Mock".to_string())]);
        assert_eq!(headers[0].0, "x-powered-by", "{headers:?}");
    }
}
