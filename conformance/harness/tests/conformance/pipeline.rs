//! Request pipeline: REQ-01 to REQ-05, REQ-07 to REQ-19 (REQ-08 and REQ-15 also
//! reference-only, Deviation D4).
//!
//! Most tables send each request and report where it ended up: which mock upstream received
//! it and with which method and target, or the gateway's status and body when no upstream
//! did. A Static Request (`x-test-static: 1`) skips Route lookup, which makes it the clearest
//! probe for URL handling and service selection.

use conformance::client::{self, Request, Response};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::{Document, MockUpstream, RecordedRequest};
use conformance::runner::{Gateway, Observations, run, run_reference_only};
use serde_json::json;

use crate::common::{assert_table, error_view, matched_route, users_gateway};

/// Where a request ended up: `"<mock label>: <METHOD> <target>"` for the first mock that
/// recorded it, or `"HTTP <status> <body>"` when none did.
pub async fn outcome(
    gw: &Gateway,
    mocks: &[(&str, &MockUpstream)],
    request: &Request,
) -> anyhow::Result<String> {
    for (_, mock) in mocks {
        mock.clear_requests();
    }
    let response = gw.send(request).await?;
    Ok(describe(mocks, &response))
}

/// Describes a response the way [`outcome`] does, for requests already sent.
pub fn describe(mocks: &[(&str, &MockUpstream)], response: &Response) -> String {
    for (label, mock) in mocks {
        if let Some(recorded) = mock.last_request() {
            return format!("{label}: {} {}", recorded.method, recorded.target);
        }
    }
    format!("HTTP {} {}", response.status, response.text())
}

/// The headers an upstream received as `name: value` lines, names lower-cased, without the
/// `x-test-*` protocol headers, and with the gateway's port in `host` masked.
pub fn header_lines(request: &RecordedRequest) -> Vec<String> {
    request
        .headers_lower()
        .into_iter()
        .map(|(n, v)| {
            let v = if n == "host" { mask_port(&v) } else { v };
            format!("{n}: {v}")
        })
        .collect()
}

/// Replaces a trailing `:<port>` of a loopback address with `:<port>`.
pub fn mask_port(value: &str) -> String {
    match value.rsplit_once(':') {
        Some((host, port))
            if (host == "127.0.0.1" || host == "[::1]") && port.parse::<u16>().is_ok() =>
        {
            format!("{host}:<port>")
        }
        _ => value.to_string(),
    }
}

fn static_get(target: &str) -> Request {
    Request::get(target).header("x-test-static", "1")
}

/// REQ-01: every request no application route or gateway endpoint claims reaches the
/// proxy, whatever its method and path.
#[tokio::test]
async fn req_01_proxy_handles_unclaimed_requests() {
    run(
        "req_01_proxy_handles_unclaimed_requests",
        &["REQ-01"],
        |target| async move {
            let users = MockUpstream::start().await?;
            users.set_document_json(fixtures::USERS_SERVICE);
            let other = MockUpstream::start().await?;
            other.set_document_json(fixtures::DEFAULT_SERVICE);
            let gw = target
                .start(
                    GatewayConfig::new()
                        .service("svc", &users)
                        .service("default", &other),
                )
                .await?;
            let mocks = [("svc", &users), ("default", &other)];
            let cases = [
                ("GET", "/svc/anything"),
                ("POST", "/x/y"),
                ("PUT", "/x"),
                ("PATCH", "/x"),
                ("DELETE", "/x"),
                ("OPTIONS", "/x"),
                ("HEAD", "/x"),
                ("GET", "/"),
                ("GET", "/mcp"),
                ("DELETE", "/mcp"),
                ("POST", "/documents"),
                ("GET", "/documentsx"),
                ("PUT", "/document-json"),
                ("GET", "/documents/extra"),
                ("GET", "/documents"),
                ("GET", "/documents/"),
                ("GET", "/DOCUMENTS"),
                ("GET", "/document-json?type=svc"),
                ("GET", "/__conformance/ready"),
            ];
            let mut observed = Vec::new();
            for (method, path) in cases {
                for (_, mock) in &mocks {
                    mock.clear_requests();
                }
                let response = gw
                    .send(&Request::new(method, path).header("x-test-static", "1"))
                    .await?;
                let mut where_to = describe(&mocks, &response);
                if where_to.starts_with("HTTP 200") {
                    where_to = format!(
                        "HTTP 200 (not forwarded) {}",
                        response.header("content-type").unwrap_or("-")
                    );
                }
                observed.push((format!("{method} {path}"), where_to));
            }
            // POST /mcp is the MCP endpoint, not the proxy.
            users.clear_requests();
            other.clear_requests();
            let mcp = conformance::mcp::send(
                gw.addr(),
                &conformance::mcp::request("tools/list", json!({})),
            )
            .await?;
            observed.push((
                "POST /mcp".into(),
                format!(
                    "HTTP {} (not forwarded: {})",
                    mcp.response.status,
                    users.requests().is_empty() && other.requests().is_empty()
                ),
            ));
            assert_table(
                "REQ-01",
                &observed,
                &[
                    ("GET /svc/anything", "svc: GET /anything"),
                    ("POST /x/y", "default: POST /x/y"),
                    ("PUT /x", "default: PUT /x"),
                    ("PATCH /x", "default: PATCH /x"),
                    ("DELETE /x", "default: DELETE /x"),
                    ("OPTIONS /x", "default: OPTIONS /x"),
                    ("HEAD /x", "default: HEAD /x"),
                    ("GET /", "default: GET /"),
                    ("GET /mcp", "default: GET /mcp"),
                    ("DELETE /mcp", "default: DELETE /mcp"),
                    ("POST /documents", "default: POST /documents"),
                    ("GET /documentsx", "default: GET /documentsx"),
                    ("PUT /document-json", "default: PUT /document-json"),
                    ("GET /documents/extra", "default: GET /documents/extra"),
                    (
                        "GET /documents",
                        "HTTP 200 (not forwarded) text/html; charset=utf-8",
                    ),
                    (
                        "GET /documents/",
                        "HTTP 200 (not forwarded) text/html; charset=utf-8",
                    ),
                    (
                        "GET /DOCUMENTS",
                        "HTTP 200 (not forwarded) text/html; charset=utf-8",
                    ),
                    (
                        "GET /document-json?type=svc",
                        "HTTP 200 (not forwarded) application/json; charset=utf-8",
                    ),
                    (
                        "GET /__conformance/ready",
                        "HTTP 200 (not forwarded) application/json; charset=utf-8",
                    ),
                    ("POST /mcp", "HTTP 200 (not forwarded: true)"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("where requests go", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-02: the URL is normalised like `new URL(url, base)` before any decision, and the
/// normalised URL is what routing and forwarding use.
#[tokio::test]
async fn req_02_url_normalisation() {
    run("req_02_url_normalisation", &["REQ-02"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mocks = [("svc", &mock)];
        // Static Requests: no Route lookup, prefix stripped, no trailing slash added.
        let cases = [
            "/svc/v1/users",
            "/svc/v1/x/../users",
            "/svc/v1/./users",
            "/svc/v1/%2e%2e/v1/users",
            "/svc/v1/.%2E/x",
            "/svc/v1/%2e/users",
            "/svc/v1/users/..",
            "/svc/v1/users/%2E%2E",
            "/svc\\v1\\users",
            "/svc/v1/users#frag",
            "/svc/v1/users?a=1#frag",
            "/svc/v1//users",
            "/svc/v1/..%2fusers",
            "/svc/v1/users;x=1",
            "/svc/%7Ev1",
            "/svc/a\"b<c>d`e{f}",
            "/svc/a^b|c'd",
            "/svc/v1/users?q='x'",
            "/../svc/v1/users",
            "/svc/../../svc/v1/users",
            "//svc/v1/users",
            "//host/svc/v1/users",
            "//host",
            "/svc//v1/users",
            "/svc//v1",
            "http://other.example/svc/v1/users?x=1",
            "http://other.example",
            "*",
            "//[bad/svc/x",
            "/svc/v1/\tusers",
        ];
        let mut observed = Vec::new();
        for case in cases {
            observed.push((
                case.to_string(),
                outcome(&gw, &mocks, &static_get(case)).await?,
            ));
        }
        // Routed Requests use the normalised URL for Route lookup.
        for case in ["/svc/x/../v1/users", "/svc/v1/users/42/../me"] {
            mock.clear_requests();
            let response = gw
                .send(&Request::get(case).header("x-test-echo-route", "1"))
                .await?;
            let where_to = match mock.last_request() {
                Some(r) => format!(
                    "routed {} -> {}",
                    matched_route(&r)["operationId"].as_str().unwrap_or("?"),
                    r.target
                ),
                None => format!("HTTP {}", response.status),
            };
            observed.push((format!("routed {case}"), where_to));
        }
        assert_table(
            "REQ-02",
            &observed,
            &[
                ("/svc/v1/users", "svc: GET /v1/users"),
                ("/svc/v1/x/../users", "svc: GET /v1/users"),
                ("/svc/v1/./users", "svc: GET /v1/users"),
                ("/svc/v1/%2e%2e/v1/users", "svc: GET /v1/users"),
                ("/svc/v1/.%2E/x", "svc: GET /x"),
                ("/svc/v1/%2e/users", "svc: GET /v1/users"),
                ("/svc/v1/users/..", "svc: GET /v1/"),
                ("/svc/v1/users/%2E%2E", "svc: GET /v1/"),
                ("/svc\\v1\\users", "svc: GET /v1/users"),
                ("/svc/v1/users#frag", "svc: GET /v1/users"),
                ("/svc/v1/users?a=1#frag", "svc: GET /v1/users?a=1"),
                ("/svc/v1//users", "svc: GET /v1//users"),
                ("/svc/v1/..%2fusers", "svc: GET /v1/..%2fusers"),
                ("/svc/v1/users;x=1", "svc: GET /v1/users;x=1"),
                ("/svc/%7Ev1", "svc: GET /%7Ev1"),
                ("/svc/a\"b<c>d`e{f}", "svc: GET /a%22b%3Cc%3Ed%60e%7Bf%7D"),
                ("/svc/a^b|c'd", "svc: GET /a%5Eb|c'd"),
                ("/svc/v1/users?q='x'", "svc: GET /v1/users?q='x'"),
                ("/../svc/v1/users", "svc: GET /v1/users"),
                ("/svc/../../svc/v1/users", "svc: GET /v1/users"),
                (
                    "//svc/v1/users",
                    "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                ),
                ("//host/svc/v1/users", "svc: GET /v1/users"),
                ("//host", "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}"),
                ("/svc//v1/users", "svc: GET /users"),
                ("/svc//v1", "svc: GET /"),
                ("http://other.example/svc/v1/users?x=1", "svc: GET /v1/users?x=1"),
                (
                    "http://other.example",
                    "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                ),
                (
                    "*",
                    "HTTP 404 {\"message\":\"Cannot GET *\",\"error\":\"Not Found\",\"statusCode\":404}",
                ),
                (
                    "//[bad/svc/x",
                    "HTTP 500 {\"statusCode\":500,\"message\":\"Internal server error\"}",
                ),
                ("/svc/v1/\tusers", "HTTP 400 "),
                ("routed /svc/x/../v1/users", "routed UsersController_listUsers -> /v1/users/"),
                (
                    "routed /svc/v1/users/42/../me",
                    "routed UsersController_getUser -> /v1/users/me/",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("normalised", &observed);
        Ok(obs)
    })
    .await;
}

/// REQ-03 (with REQ-17): `excludeHeaders` are removed, case-insensitively, from Routed,
/// Static and Bypass Requests and from WebSocket upgrades; only a middleware can set them.
#[tokio::test]
async fn req_03_excluded_headers() {
    run(
        "req_03_excluded_headers",
        &["REQ-03", "REQ-17"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.exclude_headers = vec!["Auth-User-Id".into(), "permission-in-any-guard".into()];
                c.bypass_route_prefixes = Some(vec!["svc/v1/paths".into()]);
                c
            })
            .await?;
            let spoof = |r: Request| {
                r.header("AUTH-USER-ID", "spoofed")
                    .header("auth-user-id", "spoofed-again")
                    .header("Permission-In-Any-Guard", "admin")
                    .header("x-keep", "yes")
            };
            let cases: Vec<(&str, Request)> = vec![
                ("routed", spoof(Request::get("/svc/v1/users"))),
                (
                    "routed, middleware sets auth-user-id",
                    spoof(Request::get("/svc/v1/users")).header("x-test-auth-user", "u1"),
                ),
                (
                    "routed, middleware adds authUserId and permissionInAnyGuard",
                    spoof(Request::get("/svc/v1/users")).header(
                        "x-test-add-headers",
                        r#"{"authUserId":"u2","permissionInAnyGuard":"g"}"#,
                    ),
                ),
                ("static", spoof(static_get("/svc/anything"))),
                ("bypass", spoof(Request::get("/svc/v1/paths/x"))),
            ];
            let mut observed = Vec::new();
            for (label, request) in cases {
                mock.clear_requests();
                let response = gw.send(&request).await?;
                assert_eq!(response.status, 200, "{label}");
                let recorded = mock.last_request().expect("forwarded");
                let identity: Vec<String> = header_lines(&recorded)
                    .into_iter()
                    .filter(|l| {
                        l.starts_with("auth-user-id:")
                            || l.starts_with("permission-in-any-guard:")
                            || l.starts_with("x-keep:")
                    })
                    .collect();
                observed.push((label.to_string(), identity.join(" | ")));
            }
            // A WebSocket upgrade.
            mock.set_default(conformance::mock::Behaviour::WebSocketEcho);
            mock.clear_requests();
            let upgrade =
                client::websocket(gw.addr(), &spoof(client::upgrade_request("/svc/socket")))
                    .await?;
            assert_eq!(upgrade.response_head.status, 101);
            let recorded = mock.last_request().expect("upgrade forwarded");
            observed.push(("websocket upgrade target".into(), recorded.target.clone()));
            let identity: Vec<String> = header_lines(&recorded)
                .into_iter()
                .filter(|l| {
                    l.starts_with("auth-user-id:")
                        || l.starts_with("permission-in-any-guard:")
                        || l.starts_with("x-keep:")
                })
                .collect();
            observed.push(("websocket upgrade".into(), identity.join(" | ")));
            assert_table(
                "REQ-03",
                &observed,
                &[
                    ("routed", "x-keep: yes"),
                    (
                        "routed, middleware sets auth-user-id",
                        "x-keep: yes | auth-user-id: u1",
                    ),
                    (
                        "routed, middleware adds authUserId and permissionInAnyGuard",
                        "x-keep: yes | auth-user-id: u2 | permission-in-any-guard: g",
                    ),
                    ("static", "x-keep: yes"),
                    ("bypass", "x-keep: yes"),
                    ("websocket upgrade target", "/svc/socket"),
                    ("websocket upgrade", "x-keep: yes"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("identity headers upstream", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-04: Direct Prefixes are tried first, in configuration order; on a collision the
/// first configured service owns the prefix.
#[tokio::test]
async fn req_04_direct_prefixes() {
    run("req_04_direct_prefixes", &["REQ-04"], |target| async move {
        let a = MockUpstream::start().await?;
        a.set_document_json(fixtures::DEFAULT_SERVICE);
        let b = MockUpstream::start().await?;
        b.set_document_json(fixtures::DEFAULT_SERVICE);
        let named = MockUpstream::start().await?;
        named.set_document_json(fixtures::DEFAULT_SERVICE);
        // `oauth` is also the Service Prefix of a third service, configured first.
        let gw = target
            .start(
                GatewayConfig::new()
                    .service("oauth", &named)
                    .service_with_direct("a", &a, &["oauth"])
                    .service_with_direct("b", &b, &["oauth", "health"]),
            )
            .await?;
        let mocks = [("a", &a), ("b", &b), ("oauth", &named)];
        let mut observed = Vec::new();
        for (label, request) in [
            ("routed /oauth/authorize", Request::get("/oauth/authorize")),
            (
                "routed /oauth/authorize?x=1",
                Request::get("/oauth/authorize?x=1"),
            ),
            ("routed /health", Request::get("/health")),
            ("routed /b/health", Request::get("/b/health")),
            (
                "routed /oauthx/authorize",
                Request::get("/oauthx/authorize"),
            ),
            ("static /oauth/anything", static_get("/oauth/anything")),
            ("static /oauth", static_get("/oauth")),
            ("static /health/x", static_get("/health/x")),
        ] {
            observed.push((label.to_string(), outcome(&gw, &mocks, &request).await?));
        }
        assert_table(
            "REQ-04",
            &observed,
            &[
                ("routed /oauth/authorize", "a: GET /oauth/authorize/"),
                (
                    "routed /oauth/authorize?x=1",
                    "a: GET /oauth/authorize/?x=1",
                ),
                ("routed /health", "b: GET /health/"),
                ("routed /b/health", "b: GET /health/"),
                (
                    "routed /oauthx/authorize",
                    r#"HTTP 503 {"message":"Service Unavailable","statusCode":503}"#,
                ),
                ("static /oauth/anything", "a: GET /oauth/anything"),
                ("static /oauth", "a: GET /oauth"),
                ("static /health/x", "b: GET /health/x"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("direct prefixes", &observed);
        Ok(obs)
    })
    .await;
}

/// REQ-05: Service Prefixes are tried in configuration order; the first match wins. A
/// prefix with a slash overlaps a shorter one, which makes the order observable.
#[tokio::test]
async fn req_05_service_prefix_order() {
    run(
        "req_05_service_prefix_order",
        &["REQ-05"],
        |target| async move {
            let short = MockUpstream::start().await?;
            short.set_document_json(fixtures::DEFAULT_SERVICE);
            let long = MockUpstream::start().await?;
            long.set_document_json(fixtures::DEFAULT_SERVICE);
            let mut observed = Vec::new();
            for (order, config) in [
                (
                    "api first",
                    GatewayConfig::new()
                        .service("api", &short)
                        .service("api/v2", &long),
                ),
                (
                    "api/v2 first",
                    GatewayConfig::new()
                        .service("api/v2", &long)
                        .service("api", &short),
                ),
            ] {
                let gw = target.start(config).await?;
                let mocks = [("api", &short), ("api/v2", &long)];
                for path in ["/api/v2/health", "/api/health", "/api/v2"] {
                    observed.push((
                        format!("{order}: {path}"),
                        outcome(&gw, &mocks, &static_get(path)).await?,
                    ));
                }
                observed.push((
                    format!("{order}: routed /api/v2/health"),
                    outcome(&gw, &mocks, &Request::get("/api/v2/health")).await?,
                ));
            }
            assert_table(
                "REQ-05",
                &observed,
                &[
                    ("api first: /api/v2/health", "api: GET /v2/health"),
                    ("api first: /api/health", "api: GET /health"),
                    ("api first: /api/v2", "api: GET /v2"),
                    (
                        "api first: routed /api/v2/health",
                        "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                    ),
                    ("api/v2 first: /api/v2/health", "api/v2: GET /health"),
                    ("api/v2 first: /api/health", "api: GET /health"),
                    ("api/v2 first: /api/v2", "api/v2: GET /"),
                    (
                        "api/v2 first: routed /api/v2/health",
                        "api/v2: GET /health/",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("service prefix order", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-07: the Default Service takes requests no prefix claims and requests whose Service
/// Prefix belongs to a service without a loaded document; without one the answer is 503.
#[tokio::test]
async fn req_07_default_service() {
    run("req_07_default_service", &["REQ-07"], |target| async move {
        let users = MockUpstream::start().await?;
        users.set_document_json(fixtures::USERS_SERVICE);
        let down = MockUpstream::start().await?;
        down.set_document(Document::Status(500));
        let fallback = MockUpstream::start().await?;
        fallback.set_document_json(fixtures::DEFAULT_SERVICE);
        let mut observed = Vec::new();

        let gw = target
            .start(
                GatewayConfig::new()
                    .service("svc", &users)
                    .service("down", &down)
                    .service("default", &fallback),
            )
            .await?;
        let mocks = [("svc", &users), ("down", &down), ("default", &fallback)];
        for (label, request) in [
            ("routed /health", Request::get("/health")),
            ("routed /v1/items/7", Request::get("/v1/items/7")),
            ("routed /nothing", Request::get("/nothing")),
            ("routed /down/health", Request::get("/down/health")),
            ("static /down/x", static_get("/down/x")),
            ("static /nothing/x", static_get("/nothing/x")),
            ("routed /default/health", Request::get("/default/health")),
            ("static /default/x", static_get("/default/x")),
            ("routed /svc/v1/users", Request::get("/svc/v1/users")),
        ] {
            observed.push((
                format!("with default: {label}"),
                outcome(&gw, &mocks, &request).await?,
            ));
        }
        drop(gw);

        let gw = target
            .start(
                GatewayConfig::new()
                    .service("svc", &users)
                    .service("down", &down),
            )
            .await?;
        let mocks = [("svc", &users), ("down", &down)];
        for (label, request) in [
            ("routed /health", Request::get("/health")),
            ("routed /down/health", Request::get("/down/health")),
            ("static /down/x", static_get("/down/x")),
        ] {
            observed.push((
                format!("without default: {label}"),
                outcome(&gw, &mocks, &request).await?,
            ));
        }
        assert_table(
            "REQ-07",
            &observed,
            &[
                ("with default: routed /health", "default: GET /health/"),
                (
                    "with default: routed /v1/items/7",
                    "default: GET /v1/items/7/",
                ),
                (
                    "with default: routed /nothing",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "with default: routed /down/health",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                ("with default: static /down/x", "default: GET /down/x"),
                ("with default: static /nothing/x", "default: GET /nothing/x"),
                (
                    "with default: routed /default/health",
                    "default: GET /health/",
                ),
                ("with default: static /default/x", "default: GET /x"),
                ("with default: routed /svc/v1/users", "svc: GET /v1/users/"),
                (
                    "without default: routed /health",
                    "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                ),
                (
                    "without default: routed /down/health",
                    "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                ),
                (
                    "without default: static /down/x",
                    "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("default service", &observed);
        Ok(obs)
    })
    .await;
}

/// REQ-08: a Direct Prefix match does not need the owner's document: Static and Bypass
/// Requests are still forwarded.
#[tokio::test]
async fn req_08_direct_prefix_without_document() {
    run(
        "req_08_direct_prefix_without_document",
        &["REQ-08"],
        |target| async move {
            let owner = MockUpstream::start().await?;
            owner.set_document(Document::Status(500));
            let gw = target
                .start({
                    let mut c = GatewayConfig::new().service_with_direct("own", &owner, &["oauth"]);
                    c.bypass_route_prefixes = Some(vec!["oauth/public".into()]);
                    c
                })
                .await?;
            assert_eq!(gw.loaded_services().await?, Vec::<String>::new());
            let mocks = [("own", &owner)];
            let mut observed = Vec::new();
            for (label, request) in [
                ("static /oauth/x", static_get("/oauth/x")),
                ("bypass /oauth/public/x", Request::get("/oauth/public/x")),
                ("static /own/x", static_get("/own/x")),
            ] {
                observed.push((label.to_string(), outcome(&gw, &mocks, &request).await?));
            }
            assert_table(
                "REQ-08",
                &observed,
                &[
                    ("static /oauth/x", "own: GET /oauth/x"),
                    ("bypass /oauth/public/x", "own: GET /oauth/public/x/"),
                    (
                        "static /own/x",
                        r#"HTTP 503 {"message":"Service Unavailable","statusCode":503}"#,
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("direct prefix without document", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-08 under Deviation D4: the reference answers a Routed Request to a Direct Prefix
/// owner without a loaded document with 500.
#[tokio::test]
async fn req_08_d4_routed_direct_prefix_without_document() {
    run_reference_only(
        "req_08_d4_routed_direct_prefix_without_document",
        &["REQ-08"],
        |target| async move {
            let owner = MockUpstream::start().await?;
            owner.set_document(Document::Status(500));
            let gw = target
                .start(GatewayConfig::new().service_with_direct("own", &owner, &["oauth"]))
                .await?;
            let response = gw.get("/oauth/authorize").await?;
            assert_eq!(response.status, 500);
            assert_eq!(
                response.text(),
                r#"{"statusCode":500,"message":"Internal server error"}"#
            );
            assert!(owner.requests().is_empty());
            let mut obs = Observations::new();
            obs.record(
                "routed to a Direct Prefix owner without a document",
                error_view(&response),
            );
            Ok(obs)
        },
    )
    .await;
}

/// REQ-15 under Deviation D4: what the reference answers for methods outside the eight.
#[tokio::test]
async fn req_15_d4_unsupported_methods() {
    run_reference_only(
        "req_15_d4_unsupported_methods",
        &["REQ-15"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let mut observed = Vec::new();
            for method in ["TRACE", "PROPFIND", "MKCOL", "get", "Get", "FOO", "QUERY"] {
                mock.clear_requests();
                let response = gw.send(&Request::new(method, "/svc/v1/users")).await?;
                observed.push((
                    format!("routed {method}"),
                    format!("HTTP {} {}", response.status, response.text()),
                ));
            }
            for method in ["TRACE", "PROPFIND"] {
                mock.clear_requests();
                let response = gw
                    .send(&Request::new(method, "/svc/v1/users").header("x-test-static", "1"))
                    .await?;
                observed.push((
                    format!("static {method}"),
                    describe(&[("svc", &mock)], &response),
                ));
            }
            assert_table(
                "REQ-15",
                &observed,
                &[
                    (
                        "routed TRACE",
                        "HTTP 500 {\"statusCode\":500,\"message\":\"Internal server error\"}",
                    ),
                    (
                        "routed PROPFIND",
                        "HTTP 500 {\"statusCode\":500,\"message\":\"Internal server error\"}",
                    ),
                    (
                        "routed MKCOL",
                        "HTTP 500 {\"statusCode\":500,\"message\":\"Internal server error\"}",
                    ),
                    ("routed get", "HTTP 400 "),
                    ("routed Get", "HTTP 400 "),
                    ("routed FOO", "HTTP 400 "),
                    (
                        "routed QUERY",
                        "HTTP 500 {\"statusCode\":500,\"message\":\"Internal server error\"}",
                    ),
                    ("static TRACE", "svc: TRACE /v1/users"),
                    ("static PROPFIND", "svc: PROPFIND /v1/users"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("unsupported methods", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-09, REQ-10, REQ-11: Static Requests are recognised first and forwarded with no Route
/// lookup, middleware, rate limiting or middleware headers; prefix stripped, no trailing
/// slash; service selection still applies.
#[tokio::test]
async fn req_09_static_requests() {
    run(
        "req_09_static_requests",
        &["REQ-09", "REQ-10", "REQ-11"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.bypass_route_prefixes = Some(vec!["svc/public".into()]);
                c.throttler.global_ip_rate_limit = 2.0;
                c
            })
            .await?;
            let mocks = [("svc", &mock)];
            let mut observed = Vec::new();
            for (label, request) in [
                ("no Route", static_get("/svc/no/route")),
                ("bare prefix", static_get("/svc")),
                ("already a trailing slash", static_get("/svc/v1/users/")),
                ("under a bypass prefix", static_get("/svc/public/x")),
                (
                    "middleware would deny",
                    static_get("/svc/v1/users").header("x-test-middleware", "deny"),
                ),
                (
                    "middleware would crash",
                    static_get("/svc/v1/users").header("x-test-middleware", "crash"),
                ),
                (
                    "Identity resolver would throw",
                    static_get("/svc/v1/users").header("x-test-identity-throw", "1"),
                ),
                (
                    "POST to a GET-only path",
                    Request::post("/svc/v1/users/me").header("x-test-static", "1"),
                ),
                ("no service", static_get("/nowhere/x")),
                (
                    "x-test-static: 0 is not static",
                    Request::get("/svc/no/route").header("x-test-static", "0"),
                ),
            ] {
                observed.push((label.to_string(), outcome(&gw, &mocks, &request).await?));
            }
            // No middleware headers.
            mock.clear_requests();
            gw.send(
                &static_get("/svc/v1/users")
                    .header("x-test-auth-user", "u1")
                    .header("x-test-add-headers", r#"{"xAdded":"1"}"#),
            )
            .await?;
            let lines = header_lines(&mock.last_request().expect("forwarded"));
            observed.push((
                "middleware headers".into(),
                format!(
                    "auth-user-id or x-added present: {}",
                    lines
                        .iter()
                        .any(|l| l.starts_with("auth-user-id") || l.starts_with("x-added"))
                ),
            ));
            // No rate limiting: Route limit 2 and Global IP limit 2, five requests.
            let mut statuses = Vec::new();
            for _ in 0..5 {
                statuses.push(gw.send(&static_get("/svc/v1/limits/plain")).await?.status);
            }
            observed.push((
                "five requests to a limited Route".into(),
                format!("{statuses:?}"),
            ));
            assert_table(
                "REQ-09",
                &observed,
                &[
                    ("no Route", "svc: GET /no/route"),
                    ("bare prefix", "svc: GET /"),
                    ("already a trailing slash", "svc: GET /v1/users/"),
                    ("under a bypass prefix", "svc: GET /public/x"),
                    ("middleware would deny", "svc: GET /v1/users"),
                    ("middleware would crash", "svc: GET /v1/users"),
                    ("Identity resolver would throw", "svc: GET /v1/users"),
                    ("POST to a GET-only path", "svc: POST /v1/users/me"),
                    (
                        "no service",
                        "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                    ),
                    (
                        "x-test-static: 0 is not static",
                        "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                    ),
                    (
                        "middleware headers",
                        "auth-user-id or x-added present: false",
                    ),
                    (
                        "five requests to a limited Route",
                        "[200, 200, 200, 200, 200]",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("static requests", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-12, REQ-13 (with REQ-11): Bypass Requests are matched on the normalised URL with the
/// whole-segment rule, include the Service Prefix, and are forwarded with no Route lookup,
/// middleware, rate limiting or middleware headers, with a trailing slash.
#[tokio::test]
async fn req_12_bypass_requests() {
    run(
        "req_12_bypass_requests",
        &["REQ-12", "REQ-13", "REQ-11"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.bypass_route_prefixes = Some(vec![
                    "svc/public".into(),
                    "/svc/v1/limits".into(),
                    "open".into(),
                    "nowhere".into(),
                ]);
                c.throttler.global_ip_rate_limit = 2.0;
                c
            })
            .await?;
            let mocks = [("svc", &mock)];
            let mut observed = Vec::new();
            for (label, request) in [
                ("/svc/public/x", Request::get("/svc/public/x")),
                ("/svc/public", Request::get("/svc/public")),
                ("/svc/public?q=1", Request::get("/svc/public?q=1")),
                ("/svc/public/", Request::get("/svc/public/")),
                ("/svc/publicity", Request::get("/svc/publicity")),
                ("/SVC/public/x", Request::get("/SVC/public/x")),
                ("/svc/x/../public/y", Request::get("/svc/x/../public/y")),
                (
                    "/svc/public/../v1/users",
                    Request::get("/svc/public/../v1/users"),
                ),
                ("/open/x (no service)", Request::get("/open/x")),
                (
                    "/svc/open/x (entry lacks the prefix)",
                    Request::get("/svc/open/x"),
                ),
                ("/nowhere/x", Request::get("/nowhere/x")),
                (
                    "middleware would deny",
                    Request::get("/svc/public/x").header("x-test-middleware", "deny"),
                ),
                (
                    "DELETE /svc/public/x",
                    Request::new("DELETE", "/svc/public/x"),
                ),
            ] {
                observed.push((label.to_string(), outcome(&gw, &mocks, &request).await?));
            }
            mock.clear_requests();
            gw.send(
                &Request::get("/svc/public/x")
                    .header("x-test-auth-user", "u1")
                    .header("x-test-add-headers", r#"{"xAdded":"1"}"#),
            )
            .await?;
            let lines = header_lines(&mock.last_request().expect("forwarded"));
            observed.push((
                "middleware headers".into(),
                format!(
                    "auth-user-id or x-added present: {}",
                    lines
                        .iter()
                        .any(|l| l.starts_with("auth-user-id") || l.starts_with("x-added"))
                ),
            ));
            let mut statuses = Vec::new();
            for _ in 0..5 {
                statuses.push(gw.get("/svc/v1/limits/plain").await?.status);
            }
            observed.push((
                "five requests to a limited Route".into(),
                format!("{statuses:?}"),
            ));
            assert_table(
                "REQ-12",
                &observed,
                &[
                    ("/svc/public/x", "svc: GET /public/x/"),
                    ("/svc/public", "svc: GET /public/"),
                    ("/svc/public?q=1", "svc: GET /public/?q=1"),
                    ("/svc/public/", "svc: GET /public/"),
                    (
                        "/svc/publicity",
                        "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                    ),
                    (
                        "/SVC/public/x",
                        "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                    ),
                    ("/svc/x/../public/y", "svc: GET /public/y/"),
                    ("/svc/public/../v1/users", "svc: GET /v1/users/"),
                    (
                        "/open/x (no service)",
                        "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                    ),
                    (
                        "/svc/open/x (entry lacks the prefix)",
                        "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                    ),
                    (
                        "/nowhere/x",
                        "HTTP 503 {\"message\":\"Service Unavailable\",\"statusCode\":503}",
                    ),
                    ("middleware would deny", "svc: GET /public/x/"),
                    ("DELETE /svc/public/x", "svc: DELETE /public/x/"),
                    (
                        "middleware headers",
                        "auth-user-id or x-added present: false",
                    ),
                    (
                        "five requests to a limited Route",
                        "[200, 200, 200, 200, 200]",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("bypass requests", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// REQ-14: the Route is looked up with the method and the URL minus the Service Prefix; no
/// Route answers 405, never 404.
#[tokio::test]
async fn req_14_route_lookup() {
    run("req_14_route_lookup", &["REQ-14"], |target| async move {
        let users = MockUpstream::start().await?;
        users.set_document_json(fixtures::USERS_SERVICE);
        let other = MockUpstream::start().await?;
        other.set_document_json(fixtures::DEFAULT_SERVICE);
        let gw = target
            .start(
                GatewayConfig::new()
                    .service("svc", &users)
                    .service_with_direct("default", &other, &["oauth"]),
            )
            .await?;
        let mocks = [("svc", &users), ("default", &other)];
        let mut observed = Vec::new();
        for (method, path) in [
            ("GET", "/svc/v1/users"),
            ("GET", "/svc/svc/v1/users"),
            ("GET", "/svc/nope"),
            ("GET", "/svc"),
            ("GET", "/svc/"),
            ("POST", "/svc/v1/users/42"),
            ("OPTIONS", "/svc/v1/users"),
            ("GET", "/svc?x=1"),
            ("GET", "/oauth/authorize"),
            ("POST", "/oauth/authorize"),
            ("GET", "/v1/items/9"),
            ("GET", "/default/v1/items/9"),
            ("GET", "/svc/v1/items/9"),
        ] {
            observed.push((
                format!("{method} {path}"),
                outcome(&gw, &mocks, &Request::new(method, path)).await?,
            ));
        }
        assert_table(
            "REQ-14",
            &observed,
            &[
                ("GET /svc/v1/users", "svc: GET /v1/users/"),
                (
                    "GET /svc/svc/v1/users",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "GET /svc/nope",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "GET /svc",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "GET /svc/",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "POST /svc/v1/users/42",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "OPTIONS /svc/v1/users",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                (
                    "GET /svc?x=1",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                ("GET /oauth/authorize", "default: GET /oauth/authorize/"),
                (
                    "POST /oauth/authorize",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
                ("GET /v1/items/9", "default: GET /v1/items/9/"),
                ("GET /default/v1/items/9", "default: GET /v1/items/9/"),
                (
                    "GET /svc/v1/items/9",
                    "HTTP 405 {\"message\":\"Method Not Allowed\",\"statusCode\":405}",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("route lookup", &observed);
        Ok(obs)
    })
    .await;
}

/// REQ-16: the order Route lookup, Global IP Bucket, middleware, remaining rate limits,
/// forward, shown by which requests count against which Bucket.
#[tokio::test]
async fn req_16_pipeline_order() {
    run("req_16_pipeline_order", &["REQ-16"], |target| async move {
        let mut observed = Vec::new();

        // A request without a Route never counts against the Global IP Bucket.
        let (mock, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 2.0)).await?;
        let mut statuses = Vec::new();
        for _ in 0..3 {
            statuses.push(gw.get("/svc/no/route").await?.status);
        }
        for _ in 0..3 {
            statuses.push(gw.get("/svc/v1/users").await?.status);
        }
        observed.push((
            "Global IP limit 2: three without a Route, then three with".into(),
            format!("{statuses:?}"),
        ));
        drop(gw);

        // A request rejected by middleware has counted against the Global IP Bucket.
        let (_m, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 2.0)).await?;
        let statuses = vec![
            gw.send(&Request::get("/svc/v1/users").header("x-test-middleware", "deny"))
                .await?
                .status,
            gw.get("/svc/v1/users").await?.status,
            gw.get("/svc/v1/users").await?.status,
        ];
        observed.push((
            "Global IP limit 2: denied, allowed, allowed".into(),
            format!("{statuses:?}"),
        ));
        drop(gw);

        // The Global IP Bucket is checked before middleware: a full bucket answers 429 even
        // to a request the middleware would deny.
        let (_m, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 1.0)).await?;
        let statuses = vec![
            gw.get("/svc/v1/users").await?.status,
            gw.send(&Request::get("/svc/v1/users").header("x-test-middleware", "deny"))
                .await?
                .status,
        ];
        observed.push((
            "Global IP limit 1: allowed, then a request middleware would deny".into(),
            format!("{statuses:?}"),
        ));
        drop(gw);

        // Middleware runs before the Route Bucket: denied requests do not count against it.
        let (_m, gw) = users_gateway(&target, |c| c).await?;
        let deny = Request::get("/svc/v1/limits/plain").header("x-test-middleware", "deny");
        let statuses = vec![
            gw.send(&deny).await?.status,
            gw.send(&deny).await?.status,
            gw.send(&deny).await?.status,
            gw.get("/svc/v1/limits/plain").await?.status,
            gw.get("/svc/v1/limits/plain").await?.status,
            gw.get("/svc/v1/limits/plain").await?.status,
        ];
        observed.push((
            "Route limit 2: three denied, then three allowed".into(),
            format!("{statuses:?}"),
        ));
        drop(gw);

        // Rate limits run before forwarding: a rejected request never reaches the upstream.
        mock.clear_requests();
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        for _ in 0..3 {
            gw.get("/svc/v1/limits/plain").await?;
        }
        observed.push((
            "Route limit 2: requests the upstream received out of three".into(),
            mock.requests().len().to_string(),
        ));
        assert_table(
            "REQ-16",
            &observed,
            &[
                (
                    "Global IP limit 2: three without a Route, then three with",
                    "[405, 405, 405, 200, 200, 429]",
                ),
                (
                    "Global IP limit 2: denied, allowed, allowed",
                    "[403, 200, 429]",
                ),
                (
                    "Global IP limit 1: allowed, then a request middleware would deny",
                    "[200, 429]",
                ),
                (
                    "Route limit 2: three denied, then three allowed",
                    "[403, 403, 403, 200, 200, 429]",
                ),
                (
                    "Route limit 2: requests the upstream received out of three",
                    "2",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("order", &observed);
        Ok(obs)
    })
    .await;
}

/// REQ-17, REQ-18: middleware receives the outgoing header set; a middleware that disallows
/// the request causes 403, after the Anonymous IP Bucket is counted.
#[tokio::test]
async fn req_18_middleware_denies() {
    run(
        "req_18_middleware_denies",
        &["REQ-17", "REQ-18"],
        |target| async move {
            let mut observed = Vec::new();
            let (mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.anonymous_ip_rate_limit = Some(2.0);
                    t.anonymous_ip_rate_limit_ttl = Some(60.0);
                })
            })
            .await?;
            let deny = Request::get("/svc/v1/users").header("x-test-middleware", "deny");
            for i in 1..=3 {
                mock.clear_requests();
                let response = gw.send(&deny).await?;
                observed.push((
                    format!("denied request {i}"),
                    format!(
                        "HTTP {} {} (upstream reached: {})",
                        response.status,
                        response.text(),
                        !mock.requests().is_empty()
                    ),
                ));
            }
            // Headers set by the middleware before it denies are not forwarded anywhere.
            mock.clear_requests();
            let response = gw
                .send(
                    &Request::get("/svc/v1/users")
                        .header("x-test-identity", "someone")
                        .header("x-test-identity-authenticated", "true")
                        .header("x-test-auth-user", "u1"),
                )
                .await?;
            let recorded = mock.last_request().expect("forwarded");
            observed.push((
                "allowed with x-test-auth-user".into(),
                format!(
                    "HTTP {} upstream auth-user-id: {}",
                    response.status,
                    recorded.header_all("auth-user-id").join(" | ")
                ),
            ));
            assert_table(
                "REQ-18",
                &observed,
                &[
                    (
                        "denied request 1",
                        "HTTP 403 {\"message\":\"Forbidden\",\"statusCode\":403} (upstream reached: false)",
                    ),
                    (
                        "denied request 2",
                        "HTTP 403 {\"message\":\"Forbidden\",\"statusCode\":403} (upstream reached: false)",
                    ),
                    (
                        "denied request 3",
                        "HTTP 429 {\"message\":\"error.too_many_requests\"} (upstream reached: false)",
                    ),
                    ("allowed with x-test-auth-user", "HTTP 200 upstream auth-user-id: u1"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("middleware denies", &observed);
            Ok(obs)
        },
    )
    .await;
}
