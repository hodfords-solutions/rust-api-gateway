//! HTTP forwarding: FWD-01 to FWD-18 (except FWD-16's neighbours FWD-19 and FWD-20, which
//! are Rust-only), FWD-21 to FWD-23.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use conformance::client::{self, RawConn, ReadOutcome, Request, Response};
use conformance::config::GatewayConfig;
use conformance::fixtures;
use conformance::mock::{Behaviour, MockUpstream, Reply};
use conformance::runner::{Observations, run};
use serde_json::json;

use crate::common::{assert_table, error_view, users_gateway};
use crate::pipeline::{describe, header_lines, mask_port, outcome};

/// Adds one row per header line, so long header lists stay readable.
fn push_lines(observed: &mut Vec<(String, String)>, label: &str, lines: Vec<String>) {
    for line in lines {
        observed.push((label.to_string(), line));
    }
}

fn static_request(method: &str, target: &str) -> Request {
    Request::new(method, target).header("x-test-static", "1")
}

/// Response headers as `name: value` lines in wire order, names lower-cased (their case is
/// seen, not asserted), with run-specific values masked.
fn response_lines(response: &Response) -> Vec<String> {
    response
        .headers
        .iter()
        .map(|(n, v)| {
            let n = n.to_ascii_lowercase();
            let v = match n.as_str() {
                "date" if !v.contains("2030") => "<date>".to_string(),
                _ => mask_port(v),
            };
            format!("{n}: {v}")
        })
        .collect()
}

/// FWD-04: what reaches the upstream for each query-string shape. The reference parses the
/// query with Express 5's simple parser (`node:querystring`) and undici re-serialises it.
#[tokio::test]
async fn fwd_04_query_strings() {
    run("fwd_04_query_strings", &["FWD-04"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let cases = [
            // Common shapes (must match).
            "a=1",
            "a=1&a=2",
            "a=1&b=2&a=3",
            "q=a%20b",
            "q=caf%C3%A9",
            "q=%2F%3F%26%3D%23",
            "a=",
            "a",
            "a&b=",
            "",
            // Uncommon shapes (Deviation D9: recorded, may differ).
            "q=a+b",
            "q=%2B",
            "q=%zz",
            "q=%",
            "q=%41",
            "a[b]=1",
            "a[]=1&a[]=2",
            "a%5Bb%5D=1",
            "a.b=1",
            "=v",
            "&&a=1&&",
            "a=1=2",
            "a=1;b=2",
            "A=1&a=2",
            "q=it's",
            "q=~!*()",
            "q=\"x\"",
            "q=<x>",
            "q={x}|`y`^",
            "q=a\\b",
            "__proto__=x",
            "constructor=x",
            "q=\u{1F600}",
        ];
        let mut observed = Vec::new();
        for case in cases {
            mock.clear_requests();
            let response = gw.get(&format!("/svc/v1/users?{case}")).await?;
            let forwarded = if response.status == 200 {
                let request = mock.last_request().expect("forwarded");
                match request.query() {
                    Some(q) => format!("?{q}"),
                    None => "(no query)".to_string(),
                }
            } else {
                format!("HTTP {}", response.status)
            };
            observed.push((format!("?{case}"), forwarded));
        }
        assert_table(
            "FWD-04",
            &observed,
            &[
                ("?a=1", "?a=1"),
                ("?a=1&a=2", "?a=1&a=2"),
                ("?a=1&b=2&a=3", "?a=1&a=3&b=2"),
                ("?q=a%20b", "?q=a%20b"),
                ("?q=caf%C3%A9", "?q=caf%C3%A9"),
                ("?q=%2F%3F%26%3D%23", "?q=%2F%3F%26%3D%23"),
                ("?a=", "?a="),
                ("?a", "?a="),
                ("?a&b=", "?a=&b="),
                ("?", "(no query)"),
                ("?q=a+b", "?q=a%20b"),
                ("?q=%2B", "?q=%2B"),
                ("?q=%zz", "?q=%25zz"),
                ("?q=%", "?q=%25"),
                ("?q=%41", "?q=A"),
                ("?a[b]=1", "?a%5Bb%5D=1"),
                ("?a[]=1&a[]=2", "?a%5B%5D=1&a%5B%5D=2"),
                ("?a%5Bb%5D=1", "?a%5Bb%5D=1"),
                ("?a.b=1", "?a.b=1"),
                ("?=v", "?=v"),
                ("?&&a=1&&", "?a=1"),
                ("?a=1=2", "?a=1%3D2"),
                ("?a=1;b=2", "?a=1%3Bb%3D2"),
                ("?A=1&a=2", "?A=1&a=2"),
                ("?q=it's", "?q=it's"),
                ("?q=~!*()", "?q=~!*()"),
                ("?q=\"x\"", "?q=%22x%22"),
                ("?q=<x>", "?q=%3Cx%3E"),
                ("?q={x}|`y`^", "?q=%7Bx%7D%7C%60y%60%5E"),
                ("?q=a\\b", "?q=a%5Cb"),
                ("?__proto__=x", "?__proto__=x"),
                ("?constructor=x", "?constructor=x"),
                ("?q=😀", "HTTP 400"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("query table", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-07: the `x-forwarded-for` value the upstream receives.
#[tokio::test]
async fn fwd_07_x_forwarded_for() {
    run("fwd_07_x_forwarded_for", &["FWD-07"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let v6 = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], gw.port()));
        let cases: Vec<(&str, SocketAddr, Vec<&str>)> = vec![
            ("ipv4 client, none sent", gw.addr(), vec![]),
            ("ipv4 client, one sent", gw.addr(), vec!["203.0.113.7"]),
            (
                "ipv4 client, two lines sent",
                gw.addr(),
                vec!["203.0.113.7", "198.51.100.1"],
            ),
            (
                "ipv4 client, list sent",
                gw.addr(),
                vec!["203.0.113.7, 198.51.100.1"],
            ),
            ("ipv4 client, empty sent", gw.addr(), vec![""]),
            ("ipv6 client, none sent", v6, vec![]),
        ];
        let mut observed = Vec::new();
        for (label, addr, sent) in cases {
            mock.clear_requests();
            let mut request = Request::get("/svc/v1/users");
            for value in sent {
                request = request.header("X-Forwarded-For", value);
            }
            let response = client::send(addr, &request).await?;
            assert_eq!(response.status, 200, "{label}");
            let recorded = mock.last_request().expect("forwarded");
            observed.push((
                label.to_string(),
                recorded.header_all("x-forwarded-for").join(" | "),
            ));
        }
        assert_table(
            "FWD-07",
            &observed,
            &[
                ("ipv4 client, none sent", "::ffff:127.0.0.1"),
                ("ipv4 client, one sent", "203.0.113.7, ::ffff:127.0.0.1"),
                (
                    "ipv4 client, two lines sent",
                    "203.0.113.7, 198.51.100.1, ::ffff:127.0.0.1",
                ),
                (
                    "ipv4 client, list sent",
                    "203.0.113.7, 198.51.100.1, ::ffff:127.0.0.1",
                ),
                ("ipv4 client, empty sent", "::ffff:127.0.0.1"),
                ("ipv6 client, none sent", "::1"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("x-forwarded-for", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-09: the client's `Host` header reaches the upstream unchanged.
#[tokio::test]
async fn fwd_09_host_header() {
    run("fwd_09_host_header", &["FWD-09"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut observed = Vec::new();
        for host in ["client.example:8080", "client.example", "UPPER.Example"] {
            mock.clear_requests();
            let response = gw
                .send(&Request::get("/svc/v1/users").header("Host", host))
                .await?;
            assert_eq!(response.status, 200);
            let recorded = mock.last_request().expect("forwarded");
            let hosts: Vec<String> = recorded
                .headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("host"))
                .map(|(n, v)| format!("{n}: {v}"))
                .collect();
            observed.push((host.to_string(), hosts.join(" | ")));
        }
        assert_table(
            "FWD-09",
            &observed,
            &[
                ("client.example:8080", "host: client.example:8080"),
                ("client.example", "host: client.example"),
                ("UPPER.Example", "host: UPPER.Example"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("host", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-16: the `Location` header of a 3xx response is returned unchanged.
#[tokio::test]
async fn fwd_16_location_unchanged() {
    run(
        "fwd_16_location_unchanged",
        &["FWD-16"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route("/v1/users/me", Behaviour::redirect(302, "/login"));
            mock.route(
                "/v1/users/42",
                Behaviour::redirect(301, "http://upstream.example/elsewhere"),
            );
            mock.route("/v1/users/7", Behaviour::redirect(307, "relative/path?x=1"));
            let mut observed = Vec::new();
            for path in ["/svc/v1/users/me", "/svc/v1/users/42", "/svc/v1/users/7"] {
                let response = gw.get(path).await?;
                observed.push((
                    path.to_string(),
                    format!(
                        "{} {}",
                        response.status,
                        response.header("location").unwrap_or("-")
                    ),
                ));
            }
            assert_table(
                "FWD-16",
                &observed,
                &[
                    ("/svc/v1/users/me", "302 /login"),
                    ("/svc/v1/users/42", "301 http://upstream.example/elsewhere"),
                    ("/svc/v1/users/7", "307 relative/path?x=1"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("location", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-23: the upstream fails after the response headers were sent.
#[tokio::test]
async fn fwd_23_upstream_fails_mid_body() {
    run(
        "fwd_23_upstream_fails_mid_body",
        &["FWD-23"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route(
                "/v1/users/cut-sized",
                Behaviour::CloseMidBody {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "text/plain".into()),
                        ("content-length".into(), "100".into()),
                    ],
                    body: b"0123456789".to_vec(),
                },
            );
            mock.route(
                "/v1/users/cut-chunked",
                Behaviour::CloseMidBody {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "text/plain".into()),
                        ("transfer-encoding".into(), "chunked".into()),
                    ],
                    body: b"a\r\n0123456789\r\n".to_vec(),
                },
            );
            mock.route(
                "/v1/users/ok",
                Behaviour::Respond(Reply::new(200).body("fine")),
            );
            let mut obs = Observations::new();
            for path in ["/svc/v1/users/cut-sized", "/svc/v1/users/cut-chunked"] {
                let mut conn = RawConn::connect(gw.addr()).await?;
                conn.send(&Request::get(path)).await?;
                let response = conn.read_response(false, client::DEFAULT_TIMEOUT).await?;
                let after = conn.read_some(std::time::Duration::from_secs(2)).await;
                let view = json!({
                    "status": response.status,
                    "contentLength": response.header("content-length"),
                    "transferEncoding": response.header("transfer-encoding"),
                    "body": response.text(),
                    "complete": response.complete,
                    "afterBody": format!("{after:?}"),
                });
                eprintln!("{path}: {view}");
                assert_eq!(response.status, 200, "{path}");
                assert_eq!(response.text(), "0123456789", "{path}");
                assert!(!response.complete, "{path}: the response must be cut short");
                assert_eq!(
                    after,
                    client::ReadOutcome::Closed,
                    "{path}: connection closed"
                );
                obs.record(path, view);
            }
            // The gateway keeps working afterwards.
            assert_eq!(gw.get("/svc/v1/users/ok").await?.text(), "fine");
            Ok(obs)
        },
    )
    .await;
}

/// FWD-01, FWD-02, FWD-03: the Service Prefix is removed only as the leading segment, Direct
/// Prefix and Default Service paths are kept, and every request that is not a Static
/// Request gets a trailing slash.
#[tokio::test]
async fn fwd_01_path_rewrite() {
    run(
        "fwd_01_path_rewrite",
        &["FWD-01", "FWD-02", "FWD-03"],
        |target| async move {
            let users = MockUpstream::start().await?;
            users.set_document_json(fixtures::USERS_SERVICE);
            let other = MockUpstream::start().await?;
            other.set_document_json(fixtures::DEFAULT_SERVICE);
            let mut config = GatewayConfig::new()
                .service("svc", &users)
                .service_with_direct("default", &other, &["oauth"]);
            config.bypass_route_prefixes = Some(vec!["svc/v1/paths".into(), "oauth/open".into()]);
            let gw = target.start(config).await?;
            let mocks = [("svc", &users), ("default", &other)];
            let mut observed = Vec::new();
            for (label, request) in [
                ("static /svc", static_request("GET", "/svc")),
                ("static /svc/", static_request("GET", "/svc/")),
                ("static /svc/x", static_request("GET", "/svc/x")),
                ("static /svc/x/", static_request("GET", "/svc/x/")),
                ("static /svc?x=1", static_request("GET", "/svc?x=1")),
                ("static /svc/svc/x", static_request("GET", "/svc/svc/x")),
                ("static /svcx/a", static_request("GET", "/svcx/a")),
                ("static /x/svc/a", static_request("GET", "/x/svc/a")),
                ("static /oauth/x", static_request("GET", "/oauth/x")),
                ("static /health", static_request("GET", "/health")),
                ("static /default/x", static_request("GET", "/default/x")),
                ("routed /svc/v1/users", Request::get("/svc/v1/users")),
                ("routed /svc/v1/users/", Request::get("/svc/v1/users/")),
                (
                    "routed /svc/v1/users?x=1",
                    Request::get("/svc/v1/users?x=1"),
                ),
                ("routed /svc/v1/users/42", Request::get("/svc/v1/users/42")),
                ("routed /oauth/authorize", Request::get("/oauth/authorize")),
                ("routed /health", Request::get("/health")),
                ("routed /health/", Request::get("/health/")),
                ("routed /default/health", Request::get("/default/health")),
                ("bypass /svc/v1/paths/x", Request::get("/svc/v1/paths/x")),
                ("bypass /svc/v1/paths/x/", Request::get("/svc/v1/paths/x/")),
                (
                    "bypass /svc/v1/paths?q=1",
                    Request::get("/svc/v1/paths?q=1"),
                ),
                ("bypass /oauth/open/x", Request::get("/oauth/open/x")),
            ] {
                observed.push((label.to_string(), outcome(&gw, &mocks, &request).await?));
            }
            assert_table(
                "FWD-01",
                &observed,
                &[
                    ("static /svc", "svc: GET /"),
                    ("static /svc/", "svc: GET /"),
                    ("static /svc/x", "svc: GET /x"),
                    ("static /svc/x/", "svc: GET /x/"),
                    ("static /svc?x=1", "svc: GET /?x=1"),
                    ("static /svc/svc/x", "svc: GET /svc/x"),
                    ("static /svcx/a", "default: GET /svcx/a"),
                    ("static /x/svc/a", "default: GET /x/svc/a"),
                    ("static /oauth/x", "default: GET /oauth/x"),
                    ("static /health", "default: GET /health"),
                    ("static /default/x", "default: GET /x"),
                    ("routed /svc/v1/users", "svc: GET /v1/users/"),
                    ("routed /svc/v1/users/", "svc: GET /v1/users/"),
                    ("routed /svc/v1/users?x=1", "svc: GET /v1/users/?x=1"),
                    ("routed /svc/v1/users/42", "svc: GET /v1/users/42/"),
                    ("routed /oauth/authorize", "default: GET /oauth/authorize/"),
                    ("routed /health", "default: GET /health/"),
                    ("routed /health/", "default: GET /health/"),
                    ("routed /default/health", "default: GET /health/"),
                    ("bypass /svc/v1/paths/x", "svc: GET /v1/paths/x/"),
                    ("bypass /svc/v1/paths/x/", "svc: GET /v1/paths/x/"),
                    ("bypass /svc/v1/paths?q=1", "svc: GET /v1/paths/?q=1"),
                    ("bypass /oauth/open/x", "default: GET /oauth/open/x/"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("paths", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-05: the request method is forwarded unchanged.
#[tokio::test]
async fn fwd_05_methods() {
    run("fwd_05_methods", &["FWD-05"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mocks = [("svc", &mock)];
        let mut observed = Vec::new();
        for (method, path) in [
            ("GET", "/svc/v1/users/42"),
            ("PUT", "/svc/v1/users/42"),
            ("PATCH", "/svc/v1/users/42"),
            ("DELETE", "/svc/v1/users/42"),
            ("HEAD", "/svc/v1/users/42"),
            ("POST", "/svc/v1/users"),
        ] {
            observed.push((
                format!("routed {method}"),
                outcome(&gw, &mocks, &Request::new(method, path)).await?,
            ));
        }
        for method in ["OPTIONS", "TRACE", "PROPFIND", "MKCOL", "PURGE", "QUERY"] {
            observed.push((
                format!("static {method}"),
                outcome(&gw, &mocks, &static_request(method, "/svc/x")).await?,
            ));
        }
        assert_table(
            "FWD-05",
            &observed,
            &[
                ("routed GET", "svc: GET /v1/users/42/"),
                ("routed PUT", "svc: PUT /v1/users/42/"),
                ("routed PATCH", "svc: PATCH /v1/users/42/"),
                ("routed DELETE", "svc: DELETE /v1/users/42/"),
                ("routed HEAD", "svc: HEAD /v1/users/42/"),
                ("routed POST", "svc: POST /v1/users/"),
                ("static OPTIONS", "svc: OPTIONS /x"),
                ("static TRACE", "svc: TRACE /x"),
                ("static PROPFIND", "svc: PROPFIND /x"),
                ("static MKCOL", "svc: MKCOL /x"),
                ("static PURGE", "svc: PURGE /x"),
                ("static QUERY", "svc: QUERY /x"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("methods", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-06: the fixed hop-by-hop list is not forwarded; headers named in `Connection` are.
#[tokio::test]
async fn fwd_06_hop_by_hop_request_headers() {
    run(
        "fwd_06_hop_by_hop_request_headers",
        &["FWD-06"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let hop = |r: Request| {
                r.header("Connection", "keep-alive, X-Named-Hop")
                    .header("Keep-Alive", "timeout=5")
                    .header("Proxy-Authenticate", "Basic")
                    .header("Proxy-Authorization", "Basic abc")
                    .header("TE", "trailers")
                    .header("Trailers", "X-T")
                    .header("Upgrade", "h2c")
                    .header("X-Named-Hop", "kept")
                    .header("X-After", "1")
            };
            let mut observed = Vec::new();
            mock.clear_requests();
            let response = gw.send(&hop(Request::get("/svc/v1/users"))).await?;
            assert_eq!(response.status, 200);
            let recorded = mock.last_request().expect("forwarded");
            push_lines(&mut observed, "routed GET", header_lines(&recorded));

            mock.clear_requests();
            let mut chunked = hop(static_request("POST", "/svc/upload"))
                .header("Transfer-Encoding", "chunked")
                .body(&b"4\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n"[..]);
            chunked.auto_length = false;
            let response = gw.send(&chunked).await?;
            assert_eq!(response.status, 200);
            let recorded = mock.last_request().expect("forwarded");
            // How the body is framed upstream is the HTTP client's choice (undici sends a
            // `content-length` when the whole body was already received), so it is left out.
            let lines: Vec<String> = header_lines(&recorded)
                .into_iter()
                .filter(|l| {
                    !l.starts_with("content-length:") && !l.starts_with("transfer-encoding:")
                })
                .collect();
            push_lines(&mut observed, "static chunked POST", lines);
            observed.push((
                "static chunked POST body".to_string(),
                String::from_utf8_lossy(&recorded.body).into_owned(),
            ));
            assert_table(
                "FWD-06",
                &observed,
                &[
                    ("routed GET", "host: 127.0.0.1:<port>"),
                    ("routed GET", "connection: keep-alive"),
                    ("routed GET", "x-named-hop: kept"),
                    ("routed GET", "x-after: 1"),
                    ("routed GET", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("static chunked POST", "host: 127.0.0.1:<port>"),
                    ("static chunked POST", "connection: keep-alive"),
                    ("static chunked POST", "x-named-hop: kept"),
                    ("static chunked POST", "x-after: 1"),
                    ("static chunked POST", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("static chunked POST body", "abcdefg"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("hop-by-hop", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-08: no `x-forwarded-proto`, `x-forwarded-host`, `forwarded` or `via` is added;
/// client-sent ones pass through.
#[tokio::test]
async fn fwd_08_no_added_forwarding_headers() {
    run(
        "fwd_08_no_added_forwarding_headers",
        &["FWD-08"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            let mut observed = Vec::new();
            for (label, request) in [
                ("none sent", Request::get("/svc/v1/users")),
                (
                    "client sent them",
                    Request::get("/svc/v1/users")
                        .header("X-Forwarded-Proto", "https")
                        .header("X-Forwarded-Host", "client.example")
                        .header("Forwarded", "for=192.0.2.1")
                        .header("Via", "1.1 proxy"),
                ),
            ] {
                mock.clear_requests();
                assert_eq!(gw.send(&request).await?.status, 200);
                let recorded = mock.last_request().expect("forwarded");
                push_lines(&mut observed, label, header_lines(&recorded));
            }
            assert_table(
                "FWD-08",
                &observed,
                &[
                    ("none sent", "host: 127.0.0.1:<port>"),
                    ("none sent", "connection: keep-alive"),
                    ("none sent", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("client sent them", "host: 127.0.0.1:<port>"),
                    ("client sent them", "connection: keep-alive"),
                    ("client sent them", "x-forwarded-proto: https"),
                    ("client sent them", "x-forwarded-host: client.example"),
                    ("client sent them", "forwarded: for=192.0.2.1"),
                    ("client sent them", "via: 1.1 proxy"),
                    ("client sent them", "x-forwarded-for: ::ffff:127.0.0.1"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("forwarding headers", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-10: `content-length: 0` for `DELETE` and `OPTIONS` without one; what other bodyless
/// requests carry.
#[tokio::test]
async fn fwd_10_content_length() {
    run("fwd_10_content_length", &["FWD-10"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut observed = Vec::new();
        let mut chunked_delete = Request::new("DELETE", "/svc/v1/users/42")
            .header("Transfer-Encoding", "chunked")
            .body(&b"2\r\nab\r\n0\r\n\r\n"[..]);
        chunked_delete.auto_length = false;
        for (label, request) in [
            ("routed DELETE", Request::new("DELETE", "/svc/v1/users/42")),
            (
                "routed DELETE with a body",
                Request::new("DELETE", "/svc/v1/users/42").body("abcd"),
            ),
            (
                "routed DELETE with content-length: 0",
                Request::new("DELETE", "/svc/v1/users/42").header("Content-Length", "0"),
            ),
            ("routed DELETE chunked", chunked_delete),
            ("static OPTIONS", static_request("OPTIONS", "/svc/x")),
            ("routed GET", Request::get("/svc/v1/users/42")),
            ("routed HEAD", Request::new("HEAD", "/svc/v1/users/42")),
            ("routed PUT", Request::new("PUT", "/svc/v1/users/42")),
            ("routed POST", Request::post("/svc/v1/users")),
            (
                "routed POST with a body",
                Request::post("/svc/v1/users").body("{}"),
            ),
            ("static TRACE", static_request("TRACE", "/svc/x")),
            ("static PROPFIND", static_request("PROPFIND", "/svc/x")),
        ] {
            mock.clear_requests();
            let response = gw.send(&request).await?;
            let line = match mock.last_request() {
                Some(recorded) => {
                    let framing: Vec<String> = header_lines(&recorded)
                        .into_iter()
                        .filter(|l| {
                            l.starts_with("content-length:") || l.starts_with("transfer-encoding:")
                        })
                        .collect();
                    format!(
                        "{} body {:?}",
                        framing.join(" | "),
                        String::from_utf8_lossy(&recorded.body)
                    )
                }
                None => format!("HTTP {}", response.status),
            };
            observed.push((label.to_string(), line));
        }
        assert_table(
            "FWD-10",
            &observed,
            &[
                ("routed DELETE", " body \"\""),
                (
                    "routed DELETE with a body",
                    "content-length: 4 body \"abcd\"",
                ),
                ("routed DELETE with content-length: 0", " body \"\""),
                ("routed DELETE chunked", "HTTP 503"),
                ("static OPTIONS", " body \"\""),
                ("routed GET", " body \"\""),
                ("routed HEAD", " body \"\""),
                ("routed PUT", "content-length: 0 body \"\""),
                ("routed POST", "content-length: 0 body \"\""),
                ("routed POST with a body", "content-length: 2 body \"{}\""),
                ("static TRACE", " body \"\""),
                ("static PROPFIND", "content-length: 0 body \"\""),
            ],
        );
        let mut obs = Observations::new();
        obs.record("content-length", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-11 (with REQ-21): middleware headers are applied last and replace client headers of
/// the same name; Static and Bypass Requests carry none.
#[tokio::test]
async fn fwd_11_middleware_headers() {
    run(
        "fwd_11_middleware_headers",
        &["FWD-11"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.bypass_route_prefixes = Some(vec!["svc/v1/paths".into()]);
                c
            })
            .await?;
            let added = r#"{"xTrace":"mw","xNew":"n","oauth2Token":"t","HTTPRequest":"r","x_snake":"s","accept":"mw/accept"}"#;
            let mut observed = Vec::new();
            for (label, request) in [
                ("routed", Request::get("/svc/v1/users")),
                ("static", static_request("GET", "/svc/v1/users")),
                ("bypass", Request::get("/svc/v1/paths/x")),
            ] {
                mock.clear_requests();
                let request = request
                    .header("Accept", "client/accept")
                    .header("X-Trace", "client")
                    .header("X-Last", "client")
                    .header("x-test-add-headers", added);
                assert_eq!(gw.send(&request).await?.status, 200, "{label}");
                let recorded = mock.last_request().expect("forwarded");
                push_lines(&mut observed, label, header_lines(&recorded));
            }
            // A middleware value for `host` replaces the client's.
            mock.clear_requests();
            gw.send(
                &Request::get("/svc/v1/users")
                    .header("x-test-add-headers", r#"{"host":"mw.example"}"#),
            )
            .await?;
            let recorded = mock.last_request().expect("forwarded");
            observed.push((
                "middleware sets host".into(),
                recorded.header_all("host").join(" | "),
            ));
            assert_table(
                "FWD-11",
                &observed,
                &[
                    ("routed", "host: 127.0.0.1:<port>"),
                    ("routed", "connection: keep-alive"),
                    ("routed", "accept: mw/accept"),
                    ("routed", "x-trace: mw"),
                    ("routed", "x-last: client"),
                    ("routed", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("routed", "x-new: n"),
                    ("routed", "oauth-2-token: t"),
                    ("routed", "http-request: r"),
                    ("routed", "x-snake: s"),
                    ("static", "host: 127.0.0.1:<port>"),
                    ("static", "connection: keep-alive"),
                    ("static", "accept: client/accept"),
                    ("static", "x-trace: client"),
                    ("static", "x-last: client"),
                    ("static", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("bypass", "host: 127.0.0.1:<port>"),
                    ("bypass", "connection: keep-alive"),
                    ("bypass", "accept: client/accept"),
                    ("bypass", "x-trace: client"),
                    ("bypass", "x-last: client"),
                    ("bypass", "x-forwarded-for: ::ffff:127.0.0.1"),
                    ("middleware sets host", "mw.example"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("middleware headers", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-12: other client headers, including `authorization`, `cookie` and repeated headers,
/// as the upstream receives them.
#[tokio::test]
async fn fwd_12_client_headers() {
    run("fwd_12_client_headers", &["FWD-12"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut observed = Vec::new();
        mock.clear_requests();
        let request = Request::get("/svc/v1/users")
            .header("Authorization", "Bearer first")
            .header("Cookie", "a=1")
            .header("X-Rep", "1")
            .header("Authorization", "Bearer second")
            .header("Cookie", "b=2")
            .header("X-Rep", "2")
            .header("User-Agent", "ua1")
            .header("User-Agent", "ua2")
            .header("Accept", "a")
            .header("Accept", "b")
            .header("Set-Cookie", "s=1")
            .header("Set-Cookie", "s=2")
            .header("X-Empty", "")
            .header("X-MiXeD-Case", "Value")
            .header("X-Spaces", "  padded  ")
            .header("42", "numeric name")
            .header("X-Comma", "a, b");
        assert_eq!(gw.send(&request).await?.status, 200);
        let recorded = mock.last_request().expect("forwarded");
        push_lines(&mut observed, "headers", header_lines(&recorded));
        // Node answers `Expect: 100-continue` with `100 Continue` itself; the request then
        // fails in undici, which does not support `expect`.
        mock.clear_requests();
        let mut conn = RawConn::connect(gw.addr()).await?;
        conn.send(&Request::get("/svc/v1/users").header("Expect", "100-continue"))
            .await?;
        let interim = conn.read_response(false, client::DEFAULT_TIMEOUT).await?;
        let last = conn.read_response(false, client::DEFAULT_TIMEOUT).await?;
        observed.push((
            "Expect: 100-continue".to_string(),
            format!(
                "{} then {}",
                interim.status,
                describe(&[("svc", &mock)], &last)
            ),
        ));
        assert_table(
            "FWD-12",
            &observed,
            &[
                ("headers", "host: 127.0.0.1:<port>"),
                ("headers", "connection: keep-alive"),
                ("headers", "42: numeric name"),
                ("headers", "authorization: Bearer first"),
                ("headers", "cookie: a=1; b=2"),
                ("headers", "x-rep: 1, 2"),
                ("headers", "user-agent: ua1"),
                ("headers", "accept: a, b"),
                ("headers", "set-cookie: s=1"),
                ("headers", "set-cookie: s=2"),
                ("headers", "x-empty: "),
                ("headers", "x-mixed-case: Value"),
                ("headers", "x-spaces: padded"),
                ("headers", "x-comma: a, b"),
                ("headers", "x-forwarded-for: ::ffff:127.0.0.1"),
                (
                    "Expect: 100-continue",
                    "100 then HTTP 503 {\"message\":\"Service unavailable.\"}",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("client headers", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-13: the request body is streamed to the upstream without buffering, parsing or a size
/// limit, for any content type.
#[tokio::test]
async fn fwd_13_request_body() {
    run("fwd_13_request_body", &["FWD-13"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut observed = Vec::new();
        let big: Vec<u8> = (0..16 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        for (label, content_type, body) in [
            ("16 MiB binary", "application/octet-stream", big.clone()),
            ("invalid JSON", "application/json", b"{not json".to_vec()),
            ("form", "application/x-www-form-urlencoded", b"a=1&a=2&b".to_vec()),
            ("multipart", "multipart/form-data; boundary=x", b"--x\r\nbroken".to_vec()),
        ] {
            mock.clear_requests();
            let response = gw
                .send(
                    &static_request("POST", "/svc/upload")
                        .header("Content-Type", content_type)
                        .body(body.clone()),
                )
                .await?;
            let recorded = mock.last_request().expect("forwarded");
            observed.push((
                label.to_string(),
                format!(
                    "{} identical: {} ({} bytes)",
                    response.status,
                    recorded.body == body,
                    recorded.body.len()
                ),
            ));
        }
        // Streaming: the upstream request starts before the client has sent the whole body.
        // A fresh gateway has no pooled upstream connection yet, so a new one appears only
        // when the request is dispatched.
        drop(gw);
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let before = mock.connections();
        let mut conn = RawConn::connect(gw.addr()).await?;
        conn.write(
            format!(
                "POST /svc/upload HTTP/1.1\r\nHost: {}\r\nx-test-static: 1\r\nContent-Length: 10\r\n\r\nabcde",
                gw.addr()
            )
            .as_bytes(),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let opened_early = mock.connections() > before;
        conn.write(b"fghij").await?;
        let response = conn.read_response(false, client::DEFAULT_TIMEOUT).await?;
        let recorded = mock.last_request().expect("forwarded");
        observed.push((
            "half the body, then a pause".into(),
            format!(
                "{} upstream contacted during the pause: {opened_early}, body {:?}",
                response.status,
                String::from_utf8_lossy(&recorded.body)
            ),
        ));
        assert_table(
            "FWD-13",
            &observed,
            &[
                ("16 MiB binary", "200 identical: true (16777216 bytes)"),
                ("invalid JSON", "200 identical: true (9 bytes)"),
                ("form", "200 identical: true (9 bytes)"),
                ("multipart", "200 identical: true (11 bytes)"),
                (
                    "half the body, then a pause",
                    "200 upstream contacted during the pause: true, body \"abcdefghij\"",
                ),
            ],
        );
        let mut obs = Observations::new();
        obs.record("request bodies", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-14: the upstream's status code is returned unchanged.
#[tokio::test]
async fn fwd_14_status_codes() {
    run("fwd_14_status_codes", &["FWD-14"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let codes = [
            200u16, 201, 202, 203, 206, 299, 301, 400, 401, 404, 405, 409, 418, 422, 429, 451, 500,
            502, 503, 504, 599,
        ];
        for code in codes {
            mock.route(
                &format!("/status/{code}"),
                Behaviour::Respond(Reply::new(code).body(format!("body {code}"))),
            );
        }
        let mut observed = Vec::new();
        for code in codes {
            let response = gw
                .send(&static_request("GET", &format!("/svc/status/{code}")))
                .await?;
            // Node replaces the reason phrase with its own (`unknown` for unregistered
            // codes); seen, not asserted.
            eprintln!("{code}: reason {:?}", response.reason);
            observed.push((
                code.to_string(),
                format!("{} | {}", response.status, response.text()),
            ));
        }
        assert_table(
            "FWD-14",
            &observed,
            &[
                ("200", "200 | body 200"),
                ("201", "201 | body 201"),
                ("202", "202 | body 202"),
                ("203", "203 | body 203"),
                ("206", "206 | body 206"),
                ("299", "299 | body 299"),
                ("301", "301 | body 301"),
                ("400", "400 | body 400"),
                ("401", "401 | body 401"),
                ("404", "404 | body 404"),
                ("405", "405 | body 405"),
                ("409", "409 | body 409"),
                ("418", "418 | body 418"),
                ("422", "422 | body 422"),
                ("429", "429 | body 429"),
                ("451", "451 | body 451"),
                ("500", "500 | body 500"),
                ("502", "502 | body 502"),
                ("503", "503 | body 503"),
                ("504", "504 | body 504"),
                ("599", "599 | body 599"),
            ],
        );
        let mut obs = Observations::new();
        obs.record("status codes", &observed);
        Ok(obs)
    })
    .await;
}

/// FWD-15: upstream response headers are returned unchanged except the hop-by-hop list;
/// repeated headers stay separate.
#[tokio::test]
async fn fwd_15_response_headers() {
    run(
        "fwd_15_response_headers",
        &["FWD-15"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route(
                "/v1/users/headers",
                Behaviour::Respond(
                    Reply::new(200)
                        .header("X-One", "1")
                        .header("Set-Cookie", "a=1; Path=/")
                        .header("X-Dup", "1")
                        .header("Set-Cookie", "b=2")
                        .header("X-Dup", "2")
                        .header("Keep-Alive", "timeout=9")
                        .header("Proxy-Authenticate", "Basic")
                        .header("Trailers", "X-T")
                        .header("TE", "trailers")
                        .header("Upgrade", "foo")
                        .header("Connection", "X-One")
                        .header("X-CaSe", "MiXed")
                        .header("Date", "Tue, 01 Jan 2030 00:00:00 GMT")
                        .header("Server", "mock")
                        .header("Content-Type", "text/plain")
                        .body("hello"),
                ),
            );
            mock.route(
                "/v1/users/powered",
                Behaviour::Respond(
                    Reply::new(201)
                        .header("X-Powered-By", "Mock")
                        .header("ETag", "\"abc\"")
                        .body("x"),
                ),
            );
            mock.route(
                "/v1/users/chunked",
                Behaviour::Stream {
                    status: 200,
                    headers: vec![("Content-Type".into(), "text/plain".into())],
                    chunk: b"part".to_vec(),
                    count: 2,
                    delay: Duration::ZERO,
                },
            );
            let mut observed = Vec::new();
            for path in [
                "/svc/v1/users/headers",
                "/svc/v1/users/powered",
                "/svc/v1/users/chunked",
            ] {
                let response = gw.get(path).await?;
                push_lines(&mut observed, path, response_lines(&response));
                observed.push((format!("{path} body"), response.text()));
            }
            // A client asking to close gets `Connection: close`.
            let response = gw
                .send(&Request::get("/svc/v1/users/powered").header("Connection", "close"))
                .await?;
            push_lines(
                &mut observed,
                "Connection: close request",
                response_lines(&response),
            );
            assert_table(
                "FWD-15",
                &observed,
                &[
                    ("/svc/v1/users/headers", "x-powered-by: Express"),
                    ("/svc/v1/users/headers", "x-one: 1"),
                    ("/svc/v1/users/headers", "x-dup: 1"),
                    ("/svc/v1/users/headers", "x-dup: 2"),
                    ("/svc/v1/users/headers", "x-case: MiXed"),
                    (
                        "/svc/v1/users/headers",
                        "date: Tue, 01 Jan 2030 00:00:00 GMT",
                    ),
                    ("/svc/v1/users/headers", "server: mock"),
                    ("/svc/v1/users/headers", "content-type: text/plain"),
                    ("/svc/v1/users/headers", "content-length: 5"),
                    ("/svc/v1/users/headers", "set-cookie: a=1; Path=/"),
                    ("/svc/v1/users/headers", "set-cookie: b=2"),
                    ("/svc/v1/users/headers", "connection: keep-alive"),
                    ("/svc/v1/users/headers", "keep-alive: timeout=5"),
                    ("/svc/v1/users/headers body", "hello"),
                    ("/svc/v1/users/powered", "x-powered-by: Mock"),
                    ("/svc/v1/users/powered", "etag: \"abc\""),
                    ("/svc/v1/users/powered", "content-length: 1"),
                    ("/svc/v1/users/powered", "date: <date>"),
                    ("/svc/v1/users/powered", "connection: keep-alive"),
                    ("/svc/v1/users/powered", "keep-alive: timeout=5"),
                    ("/svc/v1/users/powered body", "x"),
                    ("/svc/v1/users/chunked", "x-powered-by: Express"),
                    ("/svc/v1/users/chunked", "content-type: text/plain"),
                    ("/svc/v1/users/chunked", "date: <date>"),
                    ("/svc/v1/users/chunked", "connection: keep-alive"),
                    ("/svc/v1/users/chunked", "keep-alive: timeout=5"),
                    ("/svc/v1/users/chunked", "transfer-encoding: chunked"),
                    ("/svc/v1/users/chunked body", "partpart"),
                    ("Connection: close request", "x-powered-by: Mock"),
                    ("Connection: close request", "etag: \"abc\""),
                    ("Connection: close request", "content-length: 1"),
                    ("Connection: close request", "date: <date>"),
                    ("Connection: close request", "connection: close"),
                ],
            );
            let mut obs = Observations::new();
            obs.record("response headers", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-17: the response body is streamed as received, without decoding.
#[tokio::test]
async fn fwd_17_response_body() {
    run("fwd_17_response_body", &["FWD-17"], |target| async move {
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let gzip: Vec<u8> = vec![
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0x07, 0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
        ];
        mock.route(
            "/v1/users/gzip",
            Behaviour::Respond(
                Reply::new(200)
                    .header("Content-Type", "text/plain")
                    .header("Content-Encoding", "gzip")
                    .body(gzip.clone()),
            ),
        );
        mock.route(
            "/v1/users/slow",
            Behaviour::Stream {
                status: 200,
                headers: vec![("Content-Type".into(), "text/plain".into())],
                chunk: b"tick;".to_vec(),
                count: 3,
                delay: Duration::from_millis(700),
            },
        );
        let mut obs = Observations::new();
        let response = gw
            .send(&Request::get("/svc/v1/users/gzip").header("Accept-Encoding", "gzip"))
            .await?;
        assert_eq!(response.body, gzip, "gzip bytes unchanged");
        assert_eq!(response.header("content-encoding"), Some("gzip"));
        obs.record("gzip body unchanged", true);
        obs.record("content-encoding", response.header("content-encoding"));

        // The first chunk reaches the client before the upstream sends the last one.
        let started = Instant::now();
        let mut conn = RawConn::connect(gw.addr()).await?;
        conn.send(&Request::get("/svc/v1/users/slow")).await?;
        let mut seen = Vec::new();
        let mut first_chunk_at = None;
        while started.elapsed() < Duration::from_secs(10) {
            match conn.read_some(Duration::from_secs(5)).await {
                ReadOutcome::Data(bytes) => {
                    seen.extend_from_slice(&bytes);
                    if first_chunk_at.is_none() && String::from_utf8_lossy(&seen).contains("tick;")
                    {
                        first_chunk_at = Some(started.elapsed());
                    }
                    if String::from_utf8_lossy(&seen).ends_with("0\r\n\r\n") {
                        break;
                    }
                }
                _ => break,
            }
        }
        let total = started.elapsed();
        let first = first_chunk_at.expect("a chunk arrived");
        eprintln!("first chunk after {first:?}, complete after {total:?}");
        assert!(first < Duration::from_millis(1500), "streamed: {first:?}");
        assert!(total >= Duration::from_millis(2000), "{total:?}");
        let text = String::from_utf8_lossy(&seen).into_owned();
        let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        obs.record("first chunk before the upstream finished", true);
        obs.record("chunked body on the wire", body);
        Ok(obs)
    })
    .await;
}

/// FWD-18: responses without a body (`HEAD`, 204, 304) are forwarded with correct framing,
/// so the connection stays usable.
#[tokio::test]
async fn fwd_18_bodyless_responses() {
    run(
        "fwd_18_bodyless_responses",
        &["FWD-18"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route("/v1/users/nc", Behaviour::Respond(Reply::new(204)));
            mock.route(
                "/v1/users/nm",
                Behaviour::Respond(Reply::new(304).header("ETag", "\"v1\"")),
            );
            mock.route(
                "/v1/users/sized-head",
                Behaviour::Respond(
                    Reply::new(200)
                        .header("Content-Type", "text/plain")
                        .body("twelve bytes"),
                ),
            );
            let mut observed = Vec::new();
            let mut conn = RawConn::connect(gw.addr()).await?;
            for (method, path) in [
                ("HEAD", "/svc/v1/users/sized-head"),
                ("GET", "/svc/v1/users/nc"),
                ("GET", "/svc/v1/users/nm"),
                ("DELETE", "/svc/v1/users/nc"),
                ("GET", "/svc/v1/users/sized-head"),
            ] {
                conn.send(&Request::new(method, path)).await?;
                let response = conn
                    .read_response(method == "HEAD", client::DEFAULT_TIMEOUT)
                    .await?;
                let framing: Vec<String> = response_lines(&response)
                    .into_iter()
                    .filter(|l| {
                        let l = l.to_ascii_lowercase();
                        l.starts_with("content-length:")
                            || l.starts_with("transfer-encoding:")
                            || l.starts_with("etag:")
                    })
                    .collect();
                observed.push((
                    format!("{method} {path}"),
                    format!(
                        "{} [{}] body {:?}",
                        response.status,
                        framing.join(" | "),
                        response.text()
                    ),
                ));
            }
            assert_table(
                "FWD-18",
                &observed,
                &[
                    (
                        "HEAD /svc/v1/users/sized-head",
                        "200 [content-length: 12] body \"\"",
                    ),
                    ("GET /svc/v1/users/nc", "204 [] body \"\""),
                    ("GET /svc/v1/users/nm", "304 [etag: \"v1\"] body \"\""),
                    ("DELETE /svc/v1/users/nc", "204 [] body \"\""),
                    (
                        "GET /svc/v1/users/sized-head",
                        "200 [content-length: 12] body \"twelve bytes\"",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("bodyless responses on one connection", &observed);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-21: when the client disconnects before the response is complete, the upstream request
/// is aborted. With one pooled connection, a hung upstream request would block the next one
/// unless the abort frees (closes) the connection.
#[tokio::test]
async fn fwd_21_client_disconnect_aborts_upstream() {
    run(
        "fwd_21_client_disconnect_aborts_upstream",
        &["FWD-21"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |c| {
                c.pool(json!({ "connections": 1, "pipelining": 1 }))
            })
            .await?;
            mock.route("/v1/users/hang", Behaviour::Hang);
            let mut obs = Observations::new();
            let mut conn = RawConn::connect(gw.addr()).await?;
            conn.send(&Request::get("/svc/v1/users/hang")).await?;
            mock.wait_for_requests(1, Duration::from_secs(10)).await?;
            let connections_before = mock.connections();
            drop(conn);
            let started = Instant::now();
            let next = client::send_with_timeout(
                gw.addr(),
                &Request::get("/svc/v1/users/42"),
                Duration::from_secs(10),
            )
            .await?;
            let waited = started.elapsed();
            eprintln!("next request answered after {waited:?}");
            assert_eq!(next.status, 200);
            assert!(waited < Duration::from_secs(5), "{waited:?}");
            let new_connection = mock.connections() > connections_before;
            assert!(new_connection, "the aborted connection is not reused");
            obs.record("next request answered", next.status);
            obs.record("a new upstream connection was opened", new_connection);
            Ok(obs)
        },
    )
    .await;
}

/// FWD-22: the upstream cannot be reached or fails before sending response headers.
#[tokio::test]
async fn fwd_22_upstream_unavailable() {
    run(
        "fwd_22_upstream_unavailable",
        &["FWD-22"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            mock.route("/v1/users/close", Behaviour::CloseBeforeHeaders);
            let mut observed = Vec::new();
            for (label, request) in [
                ("closed before headers", Request::get("/svc/v1/users/close")),
                (
                    "middleware header name that is not a token",
                    Request::get("/svc/v1/users").header("x-test-add-headers", r#"{"ü":"x"}"#),
                ),
                (
                    "middleware header value with a newline",
                    Request::get("/svc/v1/users")
                        .header("x-test-add-headers", r#"{"xBad":"a\nb"}"#),
                ),
            ] {
                let response = gw.send(&request).await?;
                observed.push((
                    label.to_string(),
                    format!(
                        "{} [{}] {}",
                        response.status,
                        response.header("content-type").unwrap_or("-"),
                        response.text()
                    ),
                ));
            }
            drop(mock);
            let refused = gw.get("/svc/v1/users").await?;
            observed.push((
                "connection refused".to_string(),
                format!(
                    "{} [{}] {}",
                    refused.status,
                    refused.header("content-type").unwrap_or("-"),
                    refused.text()
                ),
            ));
            obs.record("refused, full view", error_view(&refused));
            obs.record(
                "refused, headers",
                response_lines(&refused)
                    .into_iter()
                    .filter(|l| !l.to_ascii_lowercase().starts_with("date:"))
                    .collect::<Vec<_>>(),
            );
            assert_table(
                "FWD-22",
                &observed,
                &[
                    (
                        "closed before headers",
                        "503 [application/json] {\"message\":\"Service unavailable.\"}",
                    ),
                    (
                        "middleware header name that is not a token",
                        "503 [application/json] {\"message\":\"Service unavailable.\"}",
                    ),
                    (
                        "middleware header value with a newline",
                        "503 [application/json] {\"message\":\"Service unavailable.\"}",
                    ),
                    (
                        "connection refused",
                        "503 [application/json] {\"message\":\"Service unavailable.\"}",
                    ),
                ],
            );
            obs.record("upstream unavailable", &observed);
            Ok(obs)
        },
    )
    .await;
}
