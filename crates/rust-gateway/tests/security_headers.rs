//! Security tests: header handling and request smuggling (`docs/security/attack-tests.md`).
//!
//! Requests are written as raw bytes so that malformed framing reaches the gateway as sent.

mod gateway_common;

use std::time::Duration;

use api_gateway::gateway::Gateway;
use api_gateway::handler::{
    BoxError, GatewayRequest, MiddlewareError, ProxyMiddleware, StaticRequestPredicate, async_trait,
};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use gateway_common::{
    RawResponse, Seen, Served, Upstream, options, parse_responses, raw_send, serve,
};

/// Sets the identity header the application trusts (`authUserId`, forwarded as
/// `auth-user-id`), and copies the decoded `name` query parameter into `xName`.
struct Authenticate;

#[async_trait]
impl ProxyMiddleware for Authenticate {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        proxy_request.add_header("authUserId", "u-1");
        let query = request.url().split_once('?').map_or("", |(_, q)| q);
        if let Some((_, name)) =
            url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "name")
        {
            proxy_request.add_header("xName", name.into_owned());
        }
        Ok(true)
    }
}

/// Static Requests: everything under `/svc/assets/`.
struct Assets;

#[async_trait]
impl StaticRequestPredicate for Assets {
    async fn is_static(&self, request: &GatewayRequest) -> Result<bool, BoxError> {
        Ok(request.url().starts_with("/svc/assets/"))
    }
}

/// One service `svc`, `excludeHeaders: ["auth-user-id"]`, the Bypass prefix `svc/public`,
/// Static Requests under `/svc/assets/`, and [`Authenticate`].
async fn setup() -> (Upstream, Served) {
    let upstream = Upstream::start().await;
    let mut options = options(vec![upstream.service("svc")]);
    options.bypass_route_prefixes = vec!["svc/public".into()];
    let served = serve(
        Gateway::builder(options)
            .proxy_middleware(Authenticate)
            .static_request(Assets),
        true,
    )
    .await;
    (upstream, served)
}

/// Sends raw bytes and returns every response with the requests the upstream received
/// meanwhile.
async fn exchange(
    served: &Served,
    upstream: &Upstream,
    request: &[u8],
    wait: Duration,
) -> (Vec<RawResponse>, Vec<Seen>) {
    let before = upstream.seen().len();
    let raw = raw_send(served.addr, request, wait).await;
    (parse_responses(&raw), upstream.seen()[before..].to_vec())
}

fn get(target: &str, headers: &str) -> Vec<u8> {
    format!("GET {target} HTTP/1.1\r\nHost: gw\r\n{headers}Connection: close\r\n\r\n").into_bytes()
}

fn values<'a>(seen: &'a Seen, name: &str) -> Vec<&'a str> {
    seen.headers
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
        .collect()
}

/// CRLF injection in header lines: a bare CR, a NUL, or an obsolete line folding inside the
/// head is rejected with 400 and nothing is forwarded, so no line the client hid inside
/// another reaches the upstream.
#[tokio::test]
async fn control_characters_in_header_lines_are_rejected() {
    let (upstream, served) = setup().await;
    for head in [
        "X-A: a\rX-Injected: 1\r\n",
        "X-A: a\0b\r\n",
        "X-A: a\r\n X-Injected: folded\r\n",
        "X-A: a\r\n\tX-Injected: folded\r\n",
        "X-A\r\n",
        " X-A: leading space\r\n",
        "X-A : space before colon\r\n",
    ] {
        let (responses, seen) = exchange(
            &served,
            &upstream,
            &get("/svc/v1/users", head),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            responses.first().map(|r| r.status),
            Some(400),
            "{head:?}: {responses:?}"
        );
        assert!(seen.is_empty(), "{head:?}: forwarded {seen:?}");
    }
}

/// A head that ends lines with a bare LF is never forwarded. (It is not answered either: the
/// connection is held until the client gives up; see SEC-02 for unbounded heads.)
#[tokio::test]
async fn bare_lf_line_endings_are_not_forwarded() {
    let (upstream, served) = setup().await;
    let (responses, seen) = exchange(
        &served,
        &upstream,
        b"GET /svc/v1/users HTTP/1.1\nHost: gw\nX-Injected: 1\nConnection: close\n\n",
        Duration::from_millis(700),
    )
    .await;
    assert!(responses.iter().all(|r| r.status != 200), "{responses:?}");
    assert!(seen.is_empty(), "forwarded {seen:?}");
}

/// Percent-encoded CR and LF stay encoded: in the path no Route matches, and in the query the
/// upstream receives `%0D%0A`, never a raw line break, so no header can be injected into the
/// upstream request.
#[tokio::test]
async fn encoded_crlf_never_reaches_the_upstream_decoded() {
    let (upstream, served) = setup().await;
    let (responses, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users%0d%0aX-Injected:%201", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(responses.first().map(|r| r.status), Some(405));
    assert!(seen.is_empty());

    let (responses, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users?q=%0d%0aX-Injected:%201&r=%0A", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(responses.first().map(|r| r.status), Some(200));
    assert_eq!(seen.len(), 1);
    assert!(
        !seen[0].target.contains(['\r', '\n']),
        "{:?}",
        seen[0].target
    );
    assert!(seen[0].target.contains("%0D%0A"), "{:?}", seen[0].target);
    assert!(values(&seen[0], "x-injected").is_empty());
}

/// A middleware header whose value the client controls cannot smuggle a header line
/// upstream: a value with CR or LF is refused (503) and nothing is forwarded.
#[tokio::test]
async fn middleware_header_values_with_line_breaks_are_refused() {
    let (upstream, served) = setup().await;
    let (responses, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users?name=a%0d%0aX-Injected:%201", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(
        responses.first().map(|r| r.status),
        Some(503),
        "{responses:?}"
    );
    assert!(seen.is_empty(), "forwarded {seen:?}");

    // A clean value is forwarded as the middleware set it.
    let (_, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users?name=alice", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(values(&seen[0], "x-name"), ["alice"]);
}

/// The hop-by-hop headers of FWD-06 never reach the upstream, whatever their letter case.
#[tokio::test]
async fn hop_by_hop_headers_are_not_forwarded() {
    let (upstream, served) = setup().await;
    let (responses, seen) = exchange(
        &served,
        &upstream,
        b"GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\nKeep-Alive: timeout=1\r\nTE: trailers\r\nTrailers: x-t\r\nUpgrade: h2c\r\nPROXY-AUTHORIZATION: Basic dXNlcjpwYXNz\r\nProxy-Authenticate: Basic\r\nConnection: close\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(responses.first().map(|r| r.status), Some(200));
    let names: Vec<&str> = seen[0].headers.iter().map(|(n, _)| n.as_str()).collect();
    for hop in [
        "keep-alive",
        "te",
        "trailers",
        "upgrade",
        "proxy-authorization",
        "proxy-authenticate",
        "transfer-encoding",
    ] {
        assert!(!names.contains(&hop), "{hop} forwarded: {names:?}");
    }
    // The gateway's own `connection: keep-alive` to the upstream, not the client's.
    assert_eq!(values(&seen[0], "connection"), ["keep-alive"]);
}

/// SEC-07: headers the client names in `Connection` are hop-by-hop (RFC 9110 §7.6.1) but are
/// forwarded upstream (FWD-06 pins this reference behaviour).
#[tokio::test]
#[ignore = "security finding SEC-07: headers nominated by Connection are forwarded upstream"]
async fn sec_07_headers_nominated_by_connection_are_not_forwarded() {
    let (upstream, served) = setup().await;
    let (_, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users", "Connection: x-secret\r\nX-Secret: s\r\n"),
        Duration::from_secs(2),
    )
    .await;
    assert!(values(&seen[0], "x-secret").is_empty(), "{:?}", seen[0]);
}

/// Request smuggling: for every ambiguous framing (`Content-Length` with
/// `Transfer-Encoding`, conflicting or malformed lengths, obfuscated `Transfer-Encoding`,
/// chunked HTTP/1.0, a body on `GET`), the request that follows the declared body is never
/// forwarded as a request of its own, the client gets at most one response, and the upstream
/// request is framed by the gateway (never both `content-length` and `transfer-encoding`).
#[tokio::test]
async fn ambiguous_framing_cannot_smuggle_a_request() {
    let (upstream, served) = setup().await;
    let smuggled = "GET /svc/v1/users?smuggled=1 HTTP/1.1\r\nHost: gw\r\n\r\n";
    let mut cases: Vec<(String, String)> = Vec::new();
    for framing in [
        "Content-Length: 4\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: chunked\r\nContent-Length: 4\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: xchunked\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: chunked, identity\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: identity, chunked\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: \tchunked\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: CHUNKED\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: x\r\n",
        "Content-Length: 5\r\nTransfer-Encoding: x\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 5\r\nTransfer-Encoding : chunked\r\n",
    ] {
        cases.push((
            format!("POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\n{framing}\r\n0\r\n\r\n{smuggled}"),
            framing.to_string(),
        ));
    }
    for framing in [
        "Content-Length: 4\r\nContent-Length: 60\r\n",
        "Content-Length: 4, 60\r\n",
        "Content-Length: +4\r\n",
        "Content-Length: -4\r\n",
        "Content-Length: 0x4\r\n",
        "Content-Length: 4 4\r\n",
    ] {
        cases.push((
            format!("POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\n{framing}\r\nabcd{smuggled}"),
            framing.to_string(),
        ));
    }
    cases.push((
        format!(
            "POST /svc/v1/users HTTP/1.0\r\nHost: gw\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n{smuggled}"
        ),
        "HTTP/1.0 chunked".into(),
    ));
    cases.push((
        format!(
            "GET /svc/v1/users HTTP/1.1\r\nHost: gw\r\nContent-Length: {}\r\n\r\n{smuggled}",
            smuggled.len()
        ),
        "GET with a body".into(),
    ));
    for (request, label) in cases {
        let (responses, seen) = exchange(
            &served,
            &upstream,
            request.as_bytes(),
            Duration::from_secs(2),
        )
        .await;
        assert!(responses.len() <= 1, "{label:?}: {responses:?}");
        assert!(
            seen.iter().all(|s| !s.target.contains("smuggled")),
            "{label:?}: smuggled {seen:?}"
        );
        for s in &seen {
            assert!(
                values(s, "content-length").len() + values(s, "transfer-encoding").len() <= 1,
                "{label:?}: upstream framing {s:?}"
            );
        }
    }
}

/// The gateway's own framing tracker (which holds back upgrade heads before hyper parses a
/// request) stays in step with hyper: an upgrade head carried inside a chunked body is body,
/// not an upgrade, and the next pipelined request is read where it really starts.
#[tokio::test]
async fn an_upgrade_head_inside_a_body_is_not_an_upgrade() {
    let (upstream, served) = setup().await;
    let inner = "GET /svc/ws HTTP/1.1\r\nHost: gw\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    let request = format!(
        "POST /svc/v1/users HTTP/1.1\r\nHost: gw\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{inner}\r\n0\r\n\r\nGET /svc/v1/users?after=1 HTTP/1.1\r\nHost: gw\r\nConnection: close\r\n\r\n",
        inner.len()
    );
    let (responses, seen) = exchange(
        &served,
        &upstream,
        request.as_bytes(),
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        responses.iter().map(|r| r.status).collect::<Vec<_>>(),
        [200, 200],
        "{responses:?}"
    );
    let targets: Vec<(&str, &str)> = seen
        .iter()
        .map(|s| (s.method.as_str(), s.target.as_str()))
        .collect();
    assert_eq!(
        targets,
        [("POST", "/v1/users/"), ("GET", "/v1/users/?after=1")]
    );
    assert!(seen.iter().all(|s| values(s, "upgrade").is_empty()));
}

/// The client's `X-Forwarded-For` is kept but the gateway appends the real peer, so the
/// right-most entry, which an upstream that trusts one proxy reads, cannot be spoofed.
#[tokio::test]
async fn the_last_forwarded_for_entry_is_the_real_peer() {
    let (upstream, served) = setup().await;
    let (_, seen) = exchange(
        &served,
        &upstream,
        &get(
            "/svc/v1/users",
            "X-Forwarded-For: 1.2.3.4\r\nX-Forwarded-For: 5.6.7.8, ::ffff:9.9.9.9\r\n",
        ),
        Duration::from_secs(2),
    )
    .await;
    let xff = values(&seen[0], "x-forwarded-for");
    assert_eq!(xff.len(), 1);
    assert!(xff[0].ends_with(", ::ffff:127.0.0.1"), "{xff:?}");
}

/// SEC-06: client-supplied forwarding and client-identity headers that the gateway does not
/// set itself are passed to the upstream unchanged (FWD-08), so an upstream that reads any of
/// them (for logging, rate limiting, redirects or access rules) reads the attacker's value.
#[tokio::test]
#[ignore = "security finding SEC-06: spoofed X-Real-IP / X-Forwarded-Host / X-Forwarded-Proto / Forwarded reach the upstream"]
async fn sec_06_spoofed_forwarding_headers_are_not_passed_upstream() {
    let (upstream, served) = setup().await;
    let spoofed = [
        ("x-real-ip", "1.2.3.4"),
        ("x-forwarded-host", "evil.example"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-port", "443"),
        ("forwarded", "for=1.2.3.4;host=evil.example;proto=https"),
        ("true-client-ip", "1.2.3.4"),
        ("x-client-ip", "1.2.3.4"),
    ];
    let headers: String = spoofed
        .iter()
        .map(|(n, v)| format!("{n}: {v}\r\n"))
        .collect();
    let (_, seen) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users", &headers),
        Duration::from_secs(2),
    )
    .await;
    for (name, value) in spoofed {
        assert!(
            !values(&seen[0], name).contains(&value),
            "{name}: {value} reached the upstream"
        );
    }
}

/// The identity header the application trusts (`excludeHeaders`: `auth-user-id`) cannot be
/// supplied by a client in any letter case or repeated, for Routed, Bypass and Static
/// Requests: Routed Requests carry only the middleware's value, the others none.
#[tokio::test]
async fn excluded_identity_headers_cannot_be_supplied_by_the_client() {
    let (upstream, served) = setup().await;
    let spoof = "auth-user-id: admin\r\nAuth-User-Id: admin\r\nAUTH-USER-ID: admin\r\n";
    for (target, expected) in [
        ("/svc/v1/users", vec!["u-1"]),
        ("/svc/public/x", vec![]),
        ("/svc/assets/x.js", vec![]),
    ] {
        let (responses, seen) = exchange(
            &served,
            &upstream,
            &get(target, spoof),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(responses.first().map(|r| r.status), Some(200), "{target}");
        assert_eq!(values(&seen[0], "auth-user-id"), expected, "{target}");
    }
}

/// SEC-05: an excluded header is matched by its exact name, so its underscore spelling
/// (`auth_user_id`) is forwarded. Upstreams that map `-` and `_` to the same variable
/// (CGI/WSGI and PHP `HTTP_AUTH_USER_ID`, some Ruby, Python and Go frameworks) read it as the
/// trusted identity header on Bypass and Static Requests, and on Routed Requests where no
/// middleware set it.
#[tokio::test]
#[ignore = "security finding SEC-05: the underscore spelling of an excluded identity header is forwarded"]
async fn sec_05_underscore_spellings_of_excluded_headers_are_not_forwarded() {
    let (upstream, served) = setup().await;
    for target in ["/svc/v1/users", "/svc/public/x", "/svc/assets/x.js"] {
        let (_, seen) = exchange(
            &served,
            &upstream,
            &get(target, "auth_user_id: admin\r\nAuth_User-Id: admin\r\n"),
            Duration::from_secs(2),
        )
        .await;
        let leaked: Vec<_> = seen[0]
            .headers
            .iter()
            .filter(|(n, _)| n.replace('_', "-") == "auth-user-id" && n != "auth-user-id")
            .collect();
        assert!(leaked.is_empty(), "{target}: {leaked:?}");
    }
}

/// Oversize heads are bounded: a megabyte header, a megabyte target and more than 100 header
/// lines are refused with 431 before anything is forwarded, and the gateway keeps serving.
#[tokio::test]
async fn oversize_heads_are_refused() {
    let (upstream, served) = setup().await;
    let megabyte = "a".repeat(1024 * 1024);
    let many: String = (0..200).map(|i| format!("X-H{i}: v\r\n")).collect();
    for (label, request) in [
        (
            "1 MiB header",
            get("/svc/v1/users", &format!("X-Big: {megabyte}\r\n")),
        ),
        (
            "1 MiB target",
            get(&format!("/svc/v1/users?{megabyte}"), ""),
        ),
        ("200 header lines", get("/svc/v1/users", &many)),
    ] {
        let (responses, seen) =
            exchange(&served, &upstream, &request, Duration::from_secs(3)).await;
        assert_eq!(
            responses.first().map(|r| r.status),
            Some(431),
            "{label}: {responses:?}"
        );
        assert!(seen.is_empty(), "{label}");
    }
    let (responses, _) = exchange(
        &served,
        &upstream,
        &get("/svc/v1/users", ""),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(responses.first().map(|r| r.status), Some(200));
}
