//! Security tests: path handling and upstream selection (`docs/security/attack-tests.md`).
//!
//! Requests are written as raw bytes, because an HTTP client would resolve or reject most of
//! these targets before sending them. Each test checks what the upstreams received and what
//! the client got back.

mod gateway_common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{
    BoxError, GatewayRequest, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, async_trait,
};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use gateway_common::{
    RawResponse, Served, Sink, Upstream, options, parse_responses, raw_send, serve,
};

/// Records the normalised URL of every Routed Request it sees, and lets it through.
struct Recorder(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl ProxyMiddleware for Recorder {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        self.0
            .lock()
            .expect("recorder")
            .push(request.url().to_string());
        Ok(true)
    }
}

/// The application's Static Requests: everything under `/svc/assets/`.
struct Assets;

#[async_trait]
impl StaticRequestPredicate for Assets {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        Ok(request.url().starts_with("/svc/assets/"))
    }
}

/// Two Upstream Services: `svc` (with the Bypass prefix `svc/public` and Static Requests
/// under `/svc/assets/`), and `other`, which owns the Direct Prefix `oauth`.
struct Setup {
    svc: Upstream,
    other: Upstream,
    middleware: Arc<Mutex<Vec<String>>>,
    served: Served,
}

impl Setup {
    async fn start() -> Self {
        let svc = Upstream::start().await;
        let other = Upstream::start().await;
        let mut options = options(vec![
            svc.service("svc"),
            other.service("other").with_direct_prefixes(["oauth"]),
        ]);
        options.bypass_route_prefixes = vec!["svc/public".into()];
        let middleware = Arc::new(Mutex::new(Vec::new()));
        let served = serve(
            Gateway::builder(options)
                .proxy_middleware(Recorder(Arc::clone(&middleware)))
                .static_request(Assets),
            true,
        )
        .await;
        Self {
            svc,
            other,
            middleware,
            served,
        }
    }

    /// Sends `GET <target>` as raw bytes and returns the response with what each upstream
    /// and the middleware saw for it.
    async fn get(&self, target: &str) -> Outcome {
        self.get_with(target, "Host: gw\r\n").await
    }

    async fn get_with(&self, target: &str, headers: &str) -> Outcome {
        let before = (
            self.svc.seen().len(),
            self.other.seen().len(),
            self.middleware.lock().expect("recorder").len(),
        );
        let request = format!("GET {target} HTTP/1.1\r\n{headers}Connection: close\r\n\r\n");
        let raw = raw_send(self.served.addr, request.as_bytes(), Duration::from_secs(5)).await;
        Outcome {
            response: parse_responses(&raw).into_iter().next(),
            svc: self.svc.seen()[before.0..]
                .iter()
                .map(|s| s.target.clone())
                .collect(),
            other: self.other.seen()[before.1..]
                .iter()
                .map(|s| s.target.clone())
                .collect(),
            middleware: self.middleware.lock().expect("recorder")[before.2..].to_vec(),
        }
    }
}

#[derive(Debug)]
struct Outcome {
    response: Option<RawResponse>,
    svc: Vec<String>,
    other: Vec<String>,
    middleware: Vec<String>,
}

impl Outcome {
    fn status(&self) -> u16 {
        self.response.as_ref().map_or(0, |r| r.status)
    }

    fn forwarded(&self) -> bool {
        !self.svc.is_empty() || !self.other.is_empty()
    }
}

/// Dot segments, in every spelling the URL parser resolves (`..`, `%2e%2e`, `.%2E`,
/// backslashes), are resolved before any decision. A request that climbs out of one Service
/// Prefix is therefore treated exactly like a request for the resolved URL: it is selected,
/// routed, seen by the middleware and forwarded as that URL, never as the prefix it started
/// under.
#[tokio::test]
async fn dot_segments_are_resolved_before_service_selection() {
    let setup = Setup::start().await;
    for target in [
        "/svc/../other/v1/users",
        "/svc/%2e%2e/other/v1/users",
        "/svc/%2E%2E/other/v1/users",
        "/svc/.%2E/other/v1/users",
        "/svc/%2e./other/v1/users",
        "/svc\\..\\other\\v1\\users",
        "/svc/v1/../../other/v1/users",
        "/svc/./../other/./v1/users",
        "/svc/public/../../other/v1/users",
        "/oauth/../other/v1/users",
    ] {
        let outcome = setup.get(target).await;
        assert_eq!(outcome.status(), 200, "{target}: {outcome:?}");
        assert!(outcome.svc.is_empty(), "{target}: {outcome:?}");
        assert_eq!(outcome.other, ["/v1/users/"], "{target}");
        assert_eq!(outcome.middleware, ["/other/v1/users"], "{target}");
    }
}

/// Encoded separators and dots that the URL parser does not resolve (`..%2f`, `%2e%2e%2f`,
/// double encoding, `..%5c`, `..;`), null bytes, and overlong UTF-8 never match a Route, so a
/// Routed Request carrying them is answered by the gateway and nothing is forwarded.
#[tokio::test]
async fn encoded_traversal_in_routed_requests_is_never_forwarded() {
    let setup = Setup::start().await;
    for target in [
        "/svc/v1/..%2f..%2fother/v1/users",
        "/svc/v1/..%2F..%2Fother/v1/users",
        "/svc/v1/%2e%2e%2f%2e%2e%2fother/v1/users",
        "/svc/v1/%252e%252e%252fother/v1/users",
        "/svc/v1/..%5c..%5cother/v1/users",
        "/svc/v1/users/..;/..;/other",
        "/svc/v1/users%00",
        "/svc/v1/users%00/x",
        "/svc/v1/users%2f",
        "/svc/v1/%c0%ae%c0%ae/other",
        "/svc/v1/%c0%af",
        "/svc/v1/%e0%80%ae%e0%80%ae/other",
        "/oauth/..%2fsvc/v1/users",
        "/svc/v1/secret",
    ] {
        let outcome = setup.get(target).await;
        assert!(
            matches!(outcome.status(), 400 | 404 | 405),
            "{target}: {outcome:?}"
        );
        assert!(!outcome.forwarded(), "{target}: {outcome:?}");
        assert!(outcome.middleware.is_empty(), "{target}: {outcome:?}");
    }
}

/// A raw NUL, CR or LF in the request target is rejected by the HTTP parser before the
/// gateway sees the request.
#[tokio::test]
async fn control_bytes_in_the_request_target_are_rejected() {
    let setup = Setup::start().await;
    for target in [
        "/svc/v1/users\0",
        "/svc/v1/users\0/../../other",
        "/svc/v1/users\rX-Evil: 1",
    ] {
        let outcome = setup.get(target).await;
        assert_eq!(outcome.status(), 400, "{target:?}: {outcome:?}");
        assert!(!outcome.forwarded(), "{target:?}: {outcome:?}");
    }
}

/// Dot segments cannot leave a Bypass prefix or the Static Request space either: they are
/// resolved before the request is classified, so `/svc/public/../v1/users` is a Routed
/// Request for `/svc/v1/users` and runs through the middleware.
#[tokio::test]
async fn dot_segments_cannot_escape_a_bypass_prefix_or_static_space() {
    let setup = Setup::start().await;
    for target in [
        "/svc/public/../v1/users",
        "/svc/public/%2e%2e/v1/users",
        "/svc/public\\..\\v1\\users",
        "/svc/assets/../v1/users",
        "/svc/assets/%2E%2E/v1/users",
    ] {
        let outcome = setup.get(target).await;
        assert_eq!(outcome.status(), 200, "{target}: {outcome:?}");
        assert_eq!(outcome.svc, ["/v1/users/"], "{target}");
        assert_eq!(outcome.middleware, ["/svc/v1/users"], "{target}: routed");
    }
}

/// SEC-04: under a Bypass prefix, and for Static Requests, the gateway forwards encoded
/// traversal it did not resolve (`..%2f`, `..%5c`, `..;`) as it is. An upstream that decodes
/// `%2F`/`%5C` before routing, or treats `;` as a path-parameter delimiter (Tomcat, Spring),
/// resolves `/public/..%2f..%2fadmin` to `/admin`, which the client reached without the
/// middleware (authentication) that guards Routed Requests.
///
/// Safe behaviour: no forwarded target holds a `..` segment hidden behind an encoded
/// separator or a `;` parameter.
#[tokio::test]
#[ignore = "security finding SEC-04: encoded traversal under a Bypass prefix or Static Request is forwarded raw"]
async fn sec_04_encoded_traversal_cannot_escape_a_bypass_prefix_or_static_space() {
    let setup = Setup::start().await;
    for target in [
        "/svc/public/..%2f..%2fadmin",
        "/svc/public/..%2F..%2Fadmin",
        "/svc/public/..%5c..%5cadmin",
        "/svc/public/x/..;/..;/admin",
        "/svc/assets/..%2f..%2fadmin",
        "/svc/assets/x/..;/..;/admin",
    ] {
        let outcome = setup.get(target).await;
        for forwarded in outcome.svc.iter().chain(&outcome.other) {
            let lower = forwarded.to_ascii_lowercase();
            assert!(
                !lower.contains("..%2f") && !lower.contains("..%5c") && !lower.contains("..;"),
                "{target}: forwarded as {forwarded}"
            );
        }
    }
}

/// A doubled slash after the Service Prefix makes the forwarded path drop a segment (the
/// reference reads it as an authority), but the Route is looked up on the path as received,
/// where no Route matches: the request is answered with 405 and never forwarded, so Route
/// matching and the forwarded path cannot disagree.
#[tokio::test]
async fn a_doubled_slash_cannot_make_route_and_forwarded_path_disagree() {
    let setup = Setup::start().await;
    for target in ["/svc//v1/users", "/svc//evil/v1/users", "/svc///v1/users"] {
        let outcome = setup.get(target).await;
        assert_eq!(outcome.status(), 405, "{target}: {outcome:?}");
        assert!(!outcome.forwarded(), "{target}: {outcome:?}");
    }
}

/// SSRF: nothing in the request — an absolute-form target, a scheme-relative target, user
/// info, the `Host` header, `@` in the path, a URL in the query, or an authority-form
/// `CONNECT` — makes the gateway connect anywhere but the configured upstream. The authority
/// of an absolute-form target is discarded; only its path selects the Upstream Service.
#[tokio::test]
async fn client_input_cannot_choose_the_upstream_host() {
    let setup = Setup::start().await;
    let sink = Sink::start().await;
    let at = format!("127.0.0.1:{}", sink.addr.port());
    let host = format!("Host: {at}\r\n");

    // Forwarded, to the configured `svc` upstream only.
    for target in [
        format!("http://{at}/svc/v1/users"),
        format!("https://{at}/svc/v1/users"),
        format!("//{at}/svc/v1/users"),
        format!("///{at}/svc/v1/users"),
        format!("http://user@{at}@gw/svc/v1/users"),
        format!("/svc/v1/users?url=http://{at}/&host={at}"),
    ] {
        let outcome = setup.get_with(&target, &host).await;
        assert_eq!(outcome.status(), 200, "{target}: {outcome:?}");
        assert_eq!(outcome.svc.len(), 1, "{target}: {outcome:?}");
        assert!(outcome.svc[0].starts_with("/v1/users/"), "{target}");
    }
    // Not forwarded at all.
    for target in [
        format!("http://{at}/v1/users"),
        format!("/svc/v1/users@{at}"),
        format!("@{at}/svc/v1/users"),
        at.clone(),
    ] {
        let outcome = setup.get_with(&target, &host).await;
        assert!(
            matches!(outcome.status(), 400 | 405 | 503),
            "{target}: {outcome:?}"
        );
        assert!(!outcome.forwarded(), "{target}: {outcome:?}");
    }
    let connect = format!("CONNECT {at} HTTP/1.1\r\n{host}\r\n");
    let raw = raw_send(
        setup.served.addr,
        connect.as_bytes(),
        Duration::from_secs(2),
    )
    .await;
    let response = parse_responses(&raw).into_iter().next();
    assert!(
        response.as_ref().is_some_and(|r| r.status >= 400),
        "CONNECT is refused: {response:?}"
    );
    // Give a connection the gateway might have opened time to arrive.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        sink.accepted(),
        0,
        "the gateway connected to the client's host"
    );
    assert!(setup.other.seen().is_empty());
}

/// Route matching is case-insensitive (path-to-regexp's default), so `/svc/V1/USERS` is the
/// `/v1/users` Route and runs through its middleware; the path is forwarded in the client's
/// letter case. A case variant of a path cannot reach an unguarded Route.
#[tokio::test]
async fn letter_case_variants_are_the_same_route() {
    let setup = Setup::start().await;
    let outcome = setup.get("/svc/V1/USERS").await;
    assert_eq!(outcome.status(), 200, "{outcome:?}");
    assert_eq!(outcome.middleware, ["/svc/V1/USERS"]);
    assert_eq!(outcome.svc, ["/V1/USERS/"]);
}
