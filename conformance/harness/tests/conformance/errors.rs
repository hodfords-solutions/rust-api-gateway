//! Gateway error responses: REQ-23 and REQ-24.

use std::time::{Duration, Instant};

use conformance::client::{self, Request};
use conformance::redis::RedisLink;
use conformance::runner::{Observations, run};
use serde_json::json;

use crate::common::{error_view, users_gateway};

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
/// fails the request with 500, roughly 70 s later.
#[tokio::test]
async fn req_24_redis_unreachable() {
    run(
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
