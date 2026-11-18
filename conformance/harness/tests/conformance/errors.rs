//! Gateway error responses: REQ-19, REQ-23, REQ-24 and REQ-26.

use std::time::{Duration, Instant};

use conformance::client::{self, Request};
use conformance::redis::RedisLink;
use conformance::runner::{Observations, run, run_reference_only};
use serde_json::json;

use crate::common::{assert_table, error_view, users_gateway};

/// REQ-23: the exact status, content type and body of every gateway error.
#[tokio::test]
async fn req_23_error_bodies() {
    run("req_23_error_bodies", &["REQ-23"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| c).await?;

        let forbidden = gw
            .send(&Request::get("/svc/v1/users").header("x-test-middleware", "deny"))
            .await?;
        let not_allowed = gw.send(&Request::new("PUT", "/svc/v1/users")).await?;
        let unavailable = gw.get("/no-such-service/v1/users").await?;
        let internal = gw
            .send(&Request::get("/svc/v1/users").header("x-test-identity-throw", "1"))
            .await?;
        for (label, response, status, body) in [
            ("403", &forbidden, 403, r#"{"message":"Forbidden","statusCode":403}"#),
            ("405", &not_allowed, 405, r#"{"message":"Method Not Allowed","statusCode":405}"#),
            ("503", &unavailable, 503, r#"{"message":"Service Unavailable","statusCode":503}"#),
            ("500", &internal, 500, r#"{"statusCode":500,"message":"Internal server error"}"#),
        ] {
            assert_eq!(response.status, status, "{label}");
            assert_eq!(response.text(), body, "{label}");
            assert_eq!(
                response.header("content-type"),
                Some("application/json; charset=utf-8"),
                "{label}"
            );
            obs.record(label, error_view(response));
        }

        // 429 from a full Route Bucket (RL-21): GET /v1/limits/plain allows 2 per 60 s.
        let mut route_limited = None;
        for _ in 0..3 {
            route_limited = Some(gw.get("/svc/v1/limits/plain").await?);
        }
        let route_limited = route_limited.unwrap();
        assert_eq!(route_limited.status, 429);
        assert_eq!(
            route_limited.text(),
            r#"{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"the next one minute"}}"#
        );
        obs.record("429 route", error_view(&route_limited));
        drop(gw);

        // 429 from a full Global IP Bucket (RL-20).
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| t.global_ip_rate_limit = 1.0)
        })
        .await?;
        assert_eq!(gw.get("/svc/v1/users").await?.status, 200);
        let ip_limited = gw.get("/svc/v1/users").await?;
        assert_eq!(ip_limited.status, 429);
        assert_eq!(ip_limited.text(), r#"{"message":"error.too_many_requests"}"#);
        assert_eq!(
            ip_limited.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        obs.record("429 global ip", error_view(&ip_limited));
        Ok(obs)
    })
    .await;
}

/// REQ-24: unexpected failures inside the pipeline.
#[tokio::test]
async fn req_24_unexpected_failures() {
    run(
        "req_24_unexpected_failures",
        &["REQ-24"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;

            // Identity resolver throws.
            let resolver = gw
                .send(&Request::get("/svc/v1/users").header("x-test-identity-throw", "1"))
                .await?;
            assert_eq!(resolver.status, 500);
            obs.record("identity resolver throws", error_view(&resolver));

            // A middleware throws something that is not an HTTP error.
            let crash = gw
                .send(&Request::get("/svc/v1/users").header("x-test-middleware", "crash"))
                .await?;
            assert_eq!(crash.status, 500);
            obs.record("middleware throws a plain error", error_view(&crash));

            Ok(obs)
        },
    )
    .await;
}

/// REQ-24: Redis becomes unreachable after start. The reference (ioredis 6 defaults) queues
/// the command, retries the connection 20 times with exponential backoff capped at 5 s, then
/// fails the request with 500, roughly 70 s later. Deviation D10 answers the same 500 within
/// 2 seconds, so this checks the reference only; the Rust behaviour is tested in
/// `crates/rust-gateway/tests/gateway_redis_unreachable.rs`.
#[tokio::test]
async fn req_24_redis_unreachable() {
    run_reference_only(
        "req_24_redis_unreachable",
        &["REQ-24"],
        |target| async move {
            let mut obs = Observations::new();
            let link = RedisLink::start().await?;
            let (_mock, gw) = users_gateway(&target, |mut c| {
                c.redis.host = "127.0.0.1".into();
                c.redis.port = link.addr().port();
                c
            })
            .await?;
            assert_eq!(gw.get("/svc/v1/users").await?.status, 200);
            link.sever();
            let started = Instant::now();
            let unreachable = client::send_with_timeout(
                gw.addr(),
                &Request::get("/svc/v1/users"),
                Duration::from_secs(150),
            )
            .await?;
            let waited = started.elapsed();
            eprintln!("redis unreachable: answered after {waited:?}");
            assert_eq!(unreachable.status, 500);
            assert_eq!(
                unreachable.text(),
                r#"{"statusCode":500,"message":"Internal server error"}"#
            );
            assert!(
                (Duration::from_secs(40)..Duration::from_secs(140)).contains(&waited),
                "expected the answer after the reconnection attempts, got it after {waited:?}"
            );
            obs.record("redis unreachable", error_view(&unreachable));
            obs.record("answered after reconnection attempts", json!(true));
            Ok(obs)
        },
    )
    .await;
}

/// REQ-19: a middleware fails with its own status and body. An object (or array) body is
/// returned as is; anything else is wrapped as `{"statusCode":…,"message":…}`. The Anonymous
/// IP Bucket is counted first, so a full bucket answers 429 instead.
#[tokio::test]
async fn req_19_middleware_errors() {
    run("req_19_middleware_errors", &["REQ-19"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;
        let mut observed = Vec::new();
        for (label, status, body) in [
            ("defaults", None, None),
            ("418 with an object", Some("418"), Some(r#"{"b":1,"a":"x","n":1.0}"#)),
            ("400 with a message list", Some("400"), Some(r#"{"message":["a","b"],"statusCode":400}"#)),
            ("401 with plain text", Some("401"), Some("not json")),
            ("401 with a number", Some("401"), Some("5")),
            ("401 with an array", Some("401"), Some(r#"[1,"a"]"#)),
            ("401 with a JSON string", Some("401"), Some(r#""quoted""#)),
            ("401 with null", Some("401"), Some("null")),
            ("401 with true", Some("401"), Some("true")),
            ("401 with an empty object", Some("401"), Some("{}")),
            ("500 with an object", Some("500"), Some(r#"{"message":"boom"}"#)),
            ("503 with an object", Some("503"), Some(r#"{"message":"later"}"#)),
            ("302 with an object", Some("302"), Some(r#"{"message":"moved"}"#)),
        ] {
            let mut request = Request::get("/svc/v1/users").header("x-test-middleware", "throw");
            if let Some(status) = status {
                request = request.header("x-test-error-status", status);
            }
            if let Some(body) = body {
                request = request.header("x-test-error-body", body);
            }
            mock.clear_requests();
            let response = gw.send(&request).await?;
            assert!(mock.requests().is_empty(), "{label}: not forwarded");
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
        assert_table(
            "REQ-19",
            &observed,
            &[
                (
                    "defaults",
                    "401 [application/json; charset=utf-8] {\"message\":\"test middleware error\"}",
                ),
                (
                    "418 with an object",
                    "418 [application/json; charset=utf-8] {\"b\":1,\"a\":\"x\",\"n\":1}",
                ),
                (
                    "400 with a message list",
                    "400 [application/json; charset=utf-8] {\"message\":[\"a\",\"b\"],\"statusCode\":400}",
                ),
                (
                    "401 with plain text",
                    "401 [application/json; charset=utf-8] {\"statusCode\":401,\"message\":\"not json\"}",
                ),
                (
                    "401 with a number",
                    "401 [application/json; charset=utf-8] {\"statusCode\":401,\"message\":5}",
                ),
                ("401 with an array", "401 [application/json; charset=utf-8] [1,\"a\"]"),
                (
                    "401 with a JSON string",
                    "401 [application/json; charset=utf-8] {\"statusCode\":401,\"message\":\"quoted\"}",
                ),
                (
                    "401 with null",
                    "401 [application/json; charset=utf-8] {\"statusCode\":401,\"message\":null}",
                ),
                (
                    "401 with true",
                    "401 [application/json; charset=utf-8] {\"statusCode\":401,\"message\":true}",
                ),
                ("401 with an empty object", "401 [application/json; charset=utf-8] {}"),
                (
                    "500 with an object",
                    "500 [application/json; charset=utf-8] {\"message\":\"boom\"}",
                ),
                (
                    "503 with an object",
                    "503 [application/json; charset=utf-8] {\"message\":\"later\"}",
                ),
                (
                    "302 with an object",
                    "302 [application/json; charset=utf-8] {\"message\":\"moved\"}",
                ),
            ],
        );
        obs.record("middleware errors", &observed);
        drop(gw);

        // A full Anonymous IP Bucket answers 429 instead of the middleware's error.
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| {
                t.anonymous_ip_rate_limit = Some(1.0);
                t.anonymous_ip_rate_limit_ttl = Some(60.0);
            })
        })
        .await?;
        let throw = Request::get("/svc/v1/users").header("x-test-middleware", "throw");
        let first = gw.send(&throw).await?;
        let second = gw.send(&throw).await?;
        assert_eq!(first.status, 401);
        assert_eq!(second.status, 429);
        assert_eq!(second.text(), r#"{"message":"error.too_many_requests"}"#);
        obs.record("anonymous bucket, first", error_view(&first));
        obs.record("anonymous bucket, second", error_view(&second));
        Ok(obs)
    })
    .await;
}

/// REQ-26: malformed percent-encoding in the path is rejected with 400 before any other
/// step, for every request class. The message quotes the first path segment (split on `/`)
/// that `decodeURIComponent` rejects, whole.
#[tokio::test]
async fn req_26_malformed_percent_encoding() {
    run(
        "req_26_malformed_percent_encoding",
        &["REQ-26"],
        |target| async move {
            let (mock, gw) = users_gateway(&target, |mut c| {
                c.bypass_route_prefixes = Some(vec!["svc/public".into()]);
                c
            })
            .await?;
            let mut observed = Vec::new();
            let cases: Vec<(&str, Request)> = vec![
                ("last segment", Request::get("/svc/v1/users/%zz")),
                ("earlier segment", Request::get("/svc/%zz/users")),
                ("first segment", Request::get("/%zz/svc/v1/users")),
                ("two bad segments", Request::get("/svc/a%zz/%yy")),
                ("bad then good in one segment", Request::get("/svc/v1/users/%zz%41")),
                ("good then bad in one segment", Request::get("/svc/v1/users/%41%zz")),
                ("encoded slash before the bad one", Request::get("/svc/a%2Fb%zz")),
                ("lone percent", Request::get("/svc/v1/users/%")),
                ("percent and one digit", Request::get("/svc/v1/users/%4")),
                ("invalid UTF-8", Request::get("/svc/v1/users/%e9")),
                ("truncated UTF-8", Request::get("/svc/v1/users/%F0%9F%98")),
                ("encoded surrogate", Request::get("/svc/v1/users/%ED%A0%80")),
                ("overlong encoding", Request::get("/svc/v1/users/%C0%AF")),
                ("valid UTF-8 then bad", Request::get("/svc/v1/users/%C3%A9%")),
                ("trailing slash", Request::get("/svc/v1/users/%zz/")),
                ("double slash", Request::get("//%zz")),
                ("quote in the segment", Request::get("/svc/a'b\"c%zz")),
                ("backslash in the segment", Request::get("/svc\\%zz")),
                ("bad then dot segments", Request::get("/svc/%zz/../v1/users")),
                ("in the query only", Request::get("/svc/v1/users?q=%zz")),
                ("in the fragment only", Request::get("/svc/v1/users#%zz")),
                ("in the path with a fragment", Request::get("/svc/v1/users/%zz#x")),
                ("absolute form", Request::get("http://other.example/svc/%zz")),
                ("Static Request", static_request("/svc/x/%zz")),
                ("Bypass Request", Request::get("/svc/public/%zz")),
                ("no service", Request::get("/nowhere/%zz")),
                ("WebDAV method", Request::new("PROPFIND", "/svc/%zz")),
                ("application route", Request::get("/documents/%zz")),
            ];
            for (label, request) in cases {
                mock.clear_requests();
                let response = gw.send(&request).await?;
                let forwarded = mock.last_request().map(|r| r.target);
                observed.push((
                    label.to_string(),
                    match forwarded {
                        Some(target) => format!("forwarded {target}"),
                        None => format!(
                            "{} [{}] {}",
                            response.status,
                            response.header("content-type").unwrap_or("-"),
                            response.text()
                        ),
                    },
                ));
            }
            assert_table(
                "REQ-26",
                &observed,
                &[
                    (
                        "last segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "earlier segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "first segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "two bad segments",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param 'a%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "bad then good in one segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz%41'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "good then bad in one segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%41%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "encoded slash before the bad one",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param 'a%2Fb%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "lone percent",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "percent and one digit",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%4'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "invalid UTF-8",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%e9'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "truncated UTF-8",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%F0%9F%98'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "encoded surrogate",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%ED%A0%80'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "overlong encoding",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%C0%AF'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "valid UTF-8 then bad",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%C3%A9%'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "trailing slash",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "double slash",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "quote in the segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param 'a'b\\\"c%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "backslash in the segment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param 'svc\\\\%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "bad then dot segments",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    ("in the query only", "forwarded /v1/users/?q=%25zz"),
                    ("in the fragment only", "forwarded /v1/users/"),
                    (
                        "in the path with a fragment",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "absolute form",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "Static Request",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "Bypass Request",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "no service",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "WebDAV method",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                    (
                        "application route",
                        "400 [application/json; charset=utf-8] {\"message\":\"Failed to decode param '%zz'\",\"error\":\"Bad Request\",\"statusCode\":400}",
                    ),
                ],
            );
            let mut obs = Observations::new();
            obs.record("malformed percent-encoding", &observed);
            Ok(obs)
        },
    )
    .await;
}

fn static_request(target: &str) -> Request {
    Request::get(target).header("x-test-static", "1")
}
