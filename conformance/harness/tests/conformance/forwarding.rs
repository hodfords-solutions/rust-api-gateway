//! HTTP forwarding: FWD-04, FWD-07, FWD-09, FWD-16, FWD-23.

use std::net::SocketAddr;

use conformance::client::{self, RawConn, Request};
use conformance::mock::{Behaviour, Reply};
use conformance::runner::{Observations, run};
use serde_json::json;

use crate::common::{assert_table, users_gateway};

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
