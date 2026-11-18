//! Rate limiting: docs/parity/05-rate-limiting.md.
//!
//! The client reaches the gateway from 127.0.0.1, which the default `trust proxy` setting
//! (`loopback, linklocal, uniquelocal`) trusts, so `X-Forwarded-For` picks the client
//! address a request is counted against. Each gateway start gets its own Redis database.
//!
//! The reference shares one counter among all limits of a Route (Deviation D2, RL-17), so
//! scenarios here only use Routes with a single `x-rate-limits` entry; per-limit Buckets
//! are tested Rust-only (`crates/rust-gateway/tests/rate_limit_redis_buckets.rs`).

use std::time::{Duration, Instant};

use conformance::client::{self, Request, Response};
use conformance::mock::{Behaviour, Reply};
use conformance::runner::{Observations, run};
use serde_json::{Value, json};

use crate::common::{error_view, users_gateway};

const TOO_MANY: &str = r#"{"message":"error.too_many_requests"}"#;
const ROUTE_ONE_MINUTE: &str =
    r#"{"message":"error.too_many_requests_with_custom_ttl","args":{"ttl":"the next one minute"}}"#;

/// A GET from client address `client` (via `X-Forwarded-For`).
fn from(client: &str, target: &str) -> Request {
    Request::get(target).header("x-forwarded-for", client)
}

/// Status of each response, for compact observations.
fn statuses(responses: &[Response]) -> Vec<u16> {
    responses.iter().map(|r| r.status).collect()
}

async fn send_all(
    gw: &conformance::runner::Gateway,
    requests: &[Request],
) -> anyhow::Result<Vec<Response>> {
    let mut responses = Vec::new();
    for request in requests {
        responses.push(gw.send(request).await?);
    }
    Ok(responses)
}

/// RL-01: with rate limiting disabled nothing is limited, including status-scoped limits
/// counted after the response.
#[tokio::test]
async fn rl_01_disabled() {
    run("rl_01_disabled", &["RL-01"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| {
                t.is_enable = false;
                t.global_ip_rate_limit = 1.0;
                t.global_custom_rate_limit = 1.0;
                t.anonymous_ip_rate_limit = Some(1.0);
                t.anonymous_ip_rate_limit_ttl = Some(60.0);
            })
        })
        .await?;
        mock.route("/v1/limits/on-status", Behaviour::Respond(Reply::new(201)));

        let plain = send_all(&gw, &vec![Request::get("/svc/v1/limits/plain"); 4]).await?;
        assert_eq!(statuses(&plain), [200, 200, 200, 200]);
        obs.record("GET /v1/limits/plain (limit 2) x4", statuses(&plain));

        let mut on_status = Vec::new();
        for _ in 0..3 {
            on_status.push(gw.send(&Request::post("/svc/v1/limits/on-status")).await?);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert_eq!(statuses(&on_status), [201, 201, 201]);
        obs.record(
            "POST /v1/limits/on-status (limit 1 on 201) x3",
            statuses(&on_status),
        );

        let denied = send_all(
            &gw,
            &vec![Request::get("/svc/v1/users").header("x-test-middleware", "deny"); 3],
        )
        .await?;
        assert_eq!(statuses(&denied), [403, 403, 403]);
        obs.record(
            "middleware denies x3 (anonymous limit 1)",
            statuses(&denied),
        );
        Ok(obs)
    })
    .await;
}

/// RL-02: a fixed window that does not slide. Global IP limit 2 per 1 s: the request at
/// 1.2 s opens a new window, so two more are admitted although one was admitted at 0.5 s.
#[tokio::test]
async fn rl_02_fixed_window() {
    run("rl_02_fixed_window", &["RL-02"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| {
                t.global_ip_rate_limit = 2.0;
                t.global_ip_rate_limit_ttl = 1.0;
            })
        })
        .await?;
        let start = Instant::now();
        let mut first = vec![gw.get("/svc/v1/users").await?];
        tokio::time::sleep(Duration::from_millis(500)).await;
        first.push(gw.get("/svc/v1/users").await?);
        first.push(gw.get("/svc/v1/users").await?);
        first.push(gw.get("/svc/v1/users").await?);
        assert!(start.elapsed() < Duration::from_millis(900), "timing");
        assert_eq!(statuses(&first), [200, 200, 429, 429]);
        obs.record("first window", statuses(&first));

        tokio::time::sleep_until((start + Duration::from_millis(1200)).into()).await;
        let second = send_all(&gw, &vec![Request::get("/svc/v1/users"); 3]).await?;
        assert!(start.elapsed() < Duration::from_millis(1450), "timing");
        assert_eq!(statuses(&second), [200, 200, 429]);
        obs.record("after the first window expired", statuses(&second));
        Ok(obs)
    })
    .await;
}

/// RL-03: a limit of 0 admits the request that opens the window.
#[tokio::test]
async fn rl_03_limit_zero() {
    run("rl_03_limit_zero", &["RL-03"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| t.global_custom_rate_limit = 0.0)
        })
        .await?;
        let identity = |key: &str| Request::get("/svc/v1/users").header("x-test-identity", key);
        let responses = send_all(
            &gw,
            &[identity("a"), identity("a"), identity("a"), identity("b")],
        )
        .await?;
        assert_eq!(statuses(&responses), [200, 429, 429, 200]);
        assert_eq!(responses[1].text(), TOO_MANY);
        obs.record("Custom limit 0: a, a, a, b", statuses(&responses));
        drop(gw);

        let (_mock, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 0.0)).await?;
        let responses = send_all(&gw, &vec![Request::get("/svc/v1/users"); 2]).await?;
        assert_eq!(statuses(&responses), [200, 429]);
        obs.record("Global IP limit 0", statuses(&responses));
        Ok(obs)
    })
    .await;
}

/// RL-06, RL-07: the client address comes from trusted proxies and is normalised before it
/// keys the Global IP Bucket (limit 1 here, so a 429 means "same Bucket").
#[tokio::test]
async fn rl_06_client_address() {
    run(
        "rl_06_client_address",
        &["RL-06", "RL-07"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) =
                users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 1.0)).await?;
            // Each row: X-Forwarded-For, expected status. Rows run in order on one gateway.
            let rows: &[(&str, u16)] = &[
                ("203.0.113.1", 200),
                ("203.0.113.1", 429),
                ("203.0.113.2", 200),
                // 10.0.0.5 is a trusted (unique-local) proxy: the client is 203.0.113.2.
                ("203.0.113.2, 10.0.0.5", 429),
                // 8.8.8.8 is not trusted: it is the client.
                ("203.0.113.3, 8.8.8.8", 200),
                ("8.8.8.8", 429),
                ("203.0.113.3", 200),
                // IPv6 is bucketed per /64.
                ("2001:db8:1:2::1", 200),
                ("2001:db8:1:2:ffff:ffff:ffff:ffff", 429),
                ("2001:db8:1:3::1", 200),
                // IPv4-mapped IPv6 is the IPv4 address; the zone index is dropped.
                ("::ffff:198.51.100.7", 200),
                ("198.51.100.7", 429),
                ("fe80::1%eth0", 200),
                ("fe80::2", 429),
                // Anything else is used as it is.
                ("garbage", 200),
                ("garbage", 429),
                ("Garbage", 200),
            ];
            let mut observed = Vec::new();
            for (xff, expected) in rows {
                let response = gw.send(&from(xff, "/svc/v1/users")).await?;
                assert_eq!(response.status, *expected, "X-Forwarded-For: {xff}");
                observed.push(json!([xff, response.status]));
            }
            obs.record("default trust proxy", observed);
            drop(gw);

            // No trusted proxy: X-Forwarded-For is ignored and the peer (127.0.0.1) is the client.
            let (_mock, gw) = users_gateway(&target, |mut c| {
                c.trust_proxy = Some(Value::Bool(false));
                c.throttler(|t| t.global_ip_rate_limit = 1.0)
            })
            .await?;
            let responses = send_all(
                &gw,
                &[
                    from("203.0.113.1", "/svc/v1/users"),
                    from("203.0.113.2", "/svc/v1/users"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 429]);
            obs.record("trust proxy false", statuses(&responses));
            drop(gw);

            // Hop count 1: only the peer is trusted, so the right-most entry is the client.
            let (_mock, gw) = users_gateway(&target, |mut c| {
                c.trust_proxy = Some(json!(1));
                c.throttler(|t| t.global_ip_rate_limit = 1.0)
            })
            .await?;
            let responses = send_all(
                &gw,
                &[
                    from("203.0.113.1, 203.0.113.9", "/svc/v1/users"),
                    from("203.0.113.2, 203.0.113.9", "/svc/v1/users"),
                    from("203.0.113.9, 203.0.113.1", "/svc/v1/users"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 429, 200]);
            obs.record("trust proxy 1 hop", statuses(&responses));
            drop(gw);

            // ipv6Subnet 128: one Bucket per IPv6 address.
            let (_mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.global_ip_rate_limit = 1.0;
                    t.ipv6_subnet = Some(128);
                })
            })
            .await?;
            let responses = send_all(
                &gw,
                &[
                    from("2001:db8:1:2::1", "/svc/v1/users"),
                    from("2001:db8:1:2::2", "/svc/v1/users"),
                    from("2001:db8:1:2:0:0:0:1", "/svc/v1/users"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429]);
            obs.record("ipv6Subnet 128", statuses(&responses));
            Ok(obs)
        },
    )
    .await;
}

/// RL-09: the Global IP Bucket counts every Routed Request once, before middleware, whatever
/// the Identity.
#[tokio::test]
async fn rl_09_global_ip_bucket() {
    run("rl_09_global_ip_bucket", &["RL-09"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) =
            users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 3.0)).await?;
        let responses = send_all(
            &gw,
            &[
                Request::get("/svc/v1/users").header("x-test-middleware", "deny"),
                Request::get("/svc/v1/users").header("x-test-identity", "a"),
                Request::get("/svc/v1/users").header("x-test-auth-user", "u1"),
                Request::get("/svc/v1/users").header("x-test-identity-none", "1"),
                Request::get("/svc/v1/users").header("x-test-middleware", "deny"),
            ],
        )
        .await?;
        assert_eq!(statuses(&responses), [403, 200, 200, 429, 429]);
        assert_eq!(
            responses[4].text(),
            TOO_MANY,
            "rejected before middleware runs"
        );
        obs.record("deny, key a, user u1, no key, deny", statuses(&responses));
        let other = gw.send(&from("203.0.113.50", "/svc/v1/users")).await?;
        assert_eq!(other.status, 200);
        obs.record("another address", other.status);
        Ok(obs)
    })
    .await;
}

/// RL-10: the Anonymous IP Bucket counts unauthenticated requests and requests middleware
/// rejected, once each; authenticated requests skip it.
#[tokio::test]
async fn rl_10_anonymous_ip_bucket() {
    run(
        "rl_10_anonymous_ip_bucket",
        &["RL-10"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.anonymous_ip_rate_limit = Some(2.0);
                    t.anonymous_ip_rate_limit_ttl = Some(60.0);
                })
            })
            .await?;
            let a = "203.0.113.10";
            let user =
                |client: &str| from(client, "/svc/v1/users").header("x-test-auth-user", "u1");
            let responses = send_all(
                &gw,
                &[
                    from(a, "/svc/v1/users"),
                    from(a, "/svc/v1/users"),
                    from(a, "/svc/v1/users"),
                    user(a),
                    user(a),
                    from(a, "/svc/v1/users")
                        .header("x-test-identity", "k")
                        .header("x-test-identity-authenticated", "true"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429, 200, 200, 200]);
            assert_eq!(responses[2].text(), TOO_MANY);
            obs.record("anonymous x3, authenticated x3", statuses(&responses));

            // Middleware rejections count as anonymous, and a full Bucket turns them into 429.
            let b = "203.0.113.11";
            let responses = send_all(
                &gw,
                &[
                    from(b, "/svc/v1/users").header("x-test-middleware", "deny"),
                    from(b, "/svc/v1/users").header("x-test-middleware", "throw"),
                    from(b, "/svc/v1/users").header("x-test-middleware", "deny"),
                    from(b, "/svc/v1/users").header("x-test-middleware", "throw"),
                    from(b, "/svc/v1/users"),
                    user(b),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [403, 401, 429, 429, 429, 200]);
            obs.record(
                "deny, throw, deny, throw, anonymous, authenticated",
                statuses(&responses),
            );
            obs.record(
                "full Bucket on a middleware rejection",
                error_view(&responses[3]),
            );

            // An authenticated request whose middleware rejects it still counts as anonymous.
            let c = "203.0.113.12";
            let responses = send_all(
                &gw,
                &[
                    user(c).header("x-test-middleware", "deny"),
                    user(c).header("x-test-middleware", "deny"),
                    user(c).header("x-test-middleware", "deny"),
                    user(c),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [403, 403, 429, 200]);
            obs.record(
                "authenticated but denied x3, then allowed",
                statuses(&responses),
            );
            Ok(obs)
        },
    )
    .await;
}

/// RL-11 to RL-14: the Identity, from the resolver (which runs after middleware) or the
/// address, keys the Custom Bucket; no key skips the Custom and Route Buckets.
#[tokio::test]
async fn rl_11_identity_and_custom_bucket() {
    run(
        "rl_11_identity_and_custom_bucket",
        &["RL-11", "RL-12", "RL-13", "RL-14"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.global_custom_rate_limit = 2.0;
                    t.anonymous_ip_rate_limit = Some(1.0);
                    t.anonymous_ip_rate_limit_ttl = Some(60.0);
                })
            })
            .await?;
            let key = |client: &str, key: &str| {
                from(client, "/svc/v1/users")
                    .header("x-test-identity", key)
                    .header("x-test-identity-authenticated", "true")
            };
            // RL-14: one Custom Bucket per Identity, across addresses and Routes.
            let responses = send_all(
                &gw,
                &[
                    key("203.0.113.20", "k1"),
                    from("203.0.113.21", "/svc/v1/users/me")
                        .header("x-test-identity", "k1")
                        .header("x-test-identity-authenticated", "true"),
                    key("203.0.113.22", "k1"),
                    key("203.0.113.22", "k2"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429, 200]);
            assert_eq!(responses[2].text(), TOO_MANY);
            obs.record(
                "Identity k1 x3 across addresses and Routes, then k2",
                statuses(&responses),
            );

            // RL-13: the resolver sees what middleware attached (authUserId -> user:<id>).
            let user =
                |client: &str| from(client, "/svc/v1/users").header("x-test-auth-user", "u7");
            let responses = send_all(
                &gw,
                &[
                    user("203.0.113.30"),
                    user("203.0.113.31"),
                    user("203.0.113.32"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429]);
            obs.record(
                "Identity from middleware, three addresses",
                statuses(&responses),
            );

            // RL-12: no key skips the Custom and Route Buckets (GET /v1/limits/plain allows
            // 2); authenticated, the Anonymous IP Bucket (limit 1) is skipped too.
            let none = |target: &str| {
                from("203.0.113.40", target)
                    .header("x-test-identity-none", "1")
                    .header("x-test-identity-authenticated", "true")
            };
            let responses = send_all(
                &gw,
                &[
                    none("/svc/v1/users"),
                    none("/svc/v1/users"),
                    none("/svc/v1/users"),
                    none("/svc/v1/limits/plain"),
                    none("/svc/v1/limits/plain"),
                    none("/svc/v1/limits/plain"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200; 6]);
            obs.record("no key, authenticated x6", statuses(&responses));
            // ...but an unauthenticated request without a key still meets the Anonymous IP
            // Bucket.
            let anonymous_none =
                |client: &str| from(client, "/svc/v1/users").header("x-test-identity-none", "1");
            let responses = send_all(
                &gw,
                &[
                    anonymous_none("203.0.113.41"),
                    anonymous_none("203.0.113.41"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 429]);
            obs.record("no key, anonymous x2", statuses(&responses));

            // RL-11: the resolver's default key is the normalised address.
            let ip_key = |client: &str| {
                from(client, "/svc/v1/users").header("x-test-identity-authenticated", "true")
            };
            let responses = send_all(
                &gw,
                &[
                    ip_key("::ffff:203.0.113.50"),
                    ip_key("203.0.113.50"),
                    ip_key("203.0.113.50"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429]);
            obs.record("address key, mapped then plain", statuses(&responses));
            drop(gw);

            // RL-11 without a resolver: the Identity is the address, not authenticated.
            let (_mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.key_resolver = None;
                    t.global_custom_rate_limit = 2.0;
                })
            })
            .await?;
            let responses = send_all(
                &gw,
                &[
                    from("203.0.113.60", "/svc/v1/users").header("x-test-identity", "ignored"),
                    from("203.0.113.60", "/svc/v1/users").header("x-test-auth-user", "u1"),
                    from("203.0.113.60", "/svc/v1/users"),
                    from("203.0.113.61", "/svc/v1/users"),
                ],
            )
            .await?;
            assert_eq!(statuses(&responses), [200, 200, 429, 200]);
            obs.record(
                "no resolver: same address x3, other address",
                statuses(&responses),
            );
            Ok(obs)
        },
    )
    .await;
}

/// RL-15, RL-21: a Route limit without a status, per Identity.
#[tokio::test]
async fn rl_15_route_bucket() {
    run(
        "rl_15_route_bucket",
        &["RL-15", "RL-21"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| c).await?;
            let get =
                |key: &str| Request::get("/svc/v1/limits/plain").header("x-test-identity", key);
            let responses = send_all(&gw, &[get("a"), get("a"), get("a"), get("b")]).await?;
            assert_eq!(statuses(&responses), [200, 200, 429, 200]);
            assert_eq!(responses[2].text(), ROUTE_ONE_MINUTE);
            obs.record(
                "GET /v1/limits/plain (2 per 60 s): a, a, a, b",
                statuses(&responses),
            );
            obs.record("Route rejection", error_view(&responses[2]));
            // Other Routes are not affected.
            let users = gw
                .send(&Request::get("/svc/v1/users").header("x-test-identity", "a"))
                .await?;
            assert_eq!(users.status, 200);
            obs.record("another Route, same Identity", users.status);
            Ok(obs)
        },
    )
    .await;
}

/// RL-16: a status-scoped Route limit only rejects once full, and counts after the response
/// only when the status matches.
#[tokio::test]
async fn rl_16_status_scoped_route_bucket() {
    run(
        "rl_16_status_scoped_route_bucket",
        &["RL-16", "RL-21"],
        |target| async move {
            let mut obs = Observations::new();
            let (mock, gw) = users_gateway(&target, |c| c).await?;
            // POST /v1/limits/on-status allows 1 per 60 s, counting 201 responses only.
            let post = |key: &str| {
                Request::post("/svc/v1/limits/on-status").header("x-test-identity", key)
            };
            let mut observed = Vec::new();
            mock.route("/v1/limits/on-status", Behaviour::Respond(Reply::new(400)));
            for _ in 0..3 {
                let response = gw.send(&post("a")).await?;
                assert_eq!(response.status, 400);
                observed.push(response.status);
            }
            mock.route("/v1/limits/on-status", Behaviour::Respond(Reply::new(201)));
            let counted = gw.send(&post("a")).await?;
            assert_eq!(counted.status, 201);
            observed.push(counted.status);
            // The count happens after the response has finished.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let rejected = gw.send(&post("a")).await?;
            assert_eq!(rejected.status, 429);
            assert_eq!(rejected.text(), ROUTE_ONE_MINUTE);
            observed.push(rejected.status);
            let other = gw.send(&post("b")).await?;
            assert_eq!(other.status, 201);
            observed.push(other.status);
            obs.record("400, 400, 400, 201, then a and b", observed);
            let requests = mock
                .requests()
                .iter()
                .filter(|r| r.path().starts_with("/v1/limits/on-status"))
                .count();
            assert_eq!(requests, 5, "the rejected request was not forwarded");
            obs.record("forwarded", requests);
            Ok(obs)
        },
    )
    .await;
}

/// RL-18: Global IP, middleware, Anonymous IP, Custom, then Route Buckets; the first full one
/// rejects, earlier ones have counted the request and later ones have not.
#[tokio::test]
async fn rl_18_bucket_order() {
    run("rl_18_bucket_order", &["RL-18"], |target| async move {
        let mut obs = Observations::new();
        let (_mock, gw) = users_gateway(&target, |c| {
            c.throttler(|t| {
                t.global_custom_rate_limit = 3.0;
                t.anonymous_ip_rate_limit = Some(1.0);
                t.anonymous_ip_rate_limit_ttl = Some(60.0);
            })
        })
        .await?;
        // Custom before Route: the third request fills the Route Bucket (limit 2) after the
        // Custom Bucket counted it, so the fourth meets a full Custom Bucket.
        let plain = |key: &str| {
            from("203.0.113.70", "/svc/v1/limits/plain")
                .header("x-test-identity", key)
                .header("x-test-identity-authenticated", "true")
        };
        let responses = send_all(&gw, &[plain("k"), plain("k"), plain("k"), plain("k")]).await?;
        assert_eq!(statuses(&responses), [200, 200, 429, 429]);
        assert_eq!(responses[2].text(), ROUTE_ONE_MINUTE);
        assert_eq!(responses[3].text(), TOO_MANY);
        // A forwarded response is the mock's echo, which names its port: record only the
        // status of those.
        obs.record(
            "Route full, then Custom full",
            responses
                .iter()
                .map(|r| {
                    if r.status == 200 {
                        serde_json::json!({ "status": r.status })
                    } else {
                        error_view(r)
                    }
                })
                .collect::<Vec<_>>(),
        );

        // Anonymous IP before Custom: the anonymous request rejected by the Anonymous IP
        // Bucket did not count in the Custom Bucket of its Identity.
        let anonymous =
            |key: &str| from("203.0.113.71", "/svc/v1/users").header("x-test-identity", key);
        let authenticated = |key: &str| {
            from("203.0.113.71", "/svc/v1/users")
                .header("x-test-identity", key)
                .header("x-test-identity-authenticated", "true")
        };
        let responses = send_all(
            &gw,
            &[
                anonymous("m"),
                anonymous("m"),
                anonymous("m"),
                authenticated("m"),
                authenticated("m"),
                authenticated("m"),
            ],
        )
        .await?;
        assert_eq!(statuses(&responses), [200, 429, 429, 200, 200, 429]);
        obs.record("anonymous m x3, authenticated m x3", statuses(&responses));
        Ok(obs)
    })
    .await;
}

/// RL-20, RL-21, RL-22: the 429 bodies, and no rate-limit headers.
#[tokio::test]
async fn rl_20_rejection_responses() {
    run(
        "rl_20_rejection_responses",
        &["RL-20", "RL-21", "RL-22"],
        |target| async move {
            let mut obs = Observations::new();
            let (_mock, gw) = users_gateway(&target, |c| {
                c.throttler(|t| {
                    t.global_ip_rate_limit = 4.0;
                    t.global_custom_rate_limit = 1.0;
                    t.anonymous_ip_rate_limit = Some(1.0);
                    t.anonymous_ip_rate_limit_ttl = Some(60.0);
                })
            })
            .await?;
            let mut rejections = Vec::new();
            // Custom: authenticated Identity c, twice.
            let custom = || {
                from("203.0.113.80", "/svc/v1/users")
                    .header("x-test-identity", "c")
                    .header("x-test-identity-authenticated", "true")
            };
            gw.send(&custom()).await?;
            rejections.push(("Custom", gw.send(&custom()).await?));
            // Anonymous IP: unauthenticated from one address, twice.
            gw.send(&from("203.0.113.81", "/svc/v1/users")).await?;
            rejections.push((
                "Anonymous IP",
                gw.send(&from("203.0.113.81", "/svc/v1/users")).await?,
            ));
            // Route, then Global IP, on a gateway without the low Custom limit.
            let route = || {
                from("203.0.113.82", "/svc/v1/limits/plain")
                    .header("x-test-identity", "r")
                    .header("x-test-identity-authenticated", "true")
            };
            drop(gw);
            let (_mock, gw) =
                users_gateway(&target, |c| c.throttler(|t| t.global_ip_rate_limit = 4.0)).await?;
            for _ in 0..2 {
                gw.send(&route()).await?;
            }
            rejections.push(("Route", gw.send(&route()).await?));
            // Global IP (limit 4): the fourth request is admitted by it and rejected by the
            // Route Bucket; the fifth is rejected by the Global IP Bucket.
            gw.send(&route()).await?;
            rejections.push(("Global IP", gw.send(&route()).await?));

            for (label, response) in &rejections {
                assert_eq!(response.status, 429, "{label}");
                assert_eq!(
                    response.header("content-type"),
                    Some("application/json; charset=utf-8"),
                    "{label}"
                );
                let expected = if *label == "Route" {
                    ROUTE_ONE_MINUTE
                } else {
                    TOO_MANY
                };
                assert_eq!(response.text(), expected, "{label}");
                for (name, _) in &response.headers {
                    let name = name.to_ascii_lowercase();
                    assert!(
                        name != "retry-after"
                            && !name.starts_with("x-ratelimit")
                            && !name.starts_with("ratelimit"),
                        "{label}: unexpected header {name}"
                    );
                }
                obs.record(*label, error_view(response));
            }
            Ok(obs)
        },
    )
    .await;
}

/// RL-24: Static Requests, Bypass Requests, WebSocket upgrades and requests with no Route are
/// never rate limited.
#[tokio::test]
async fn rl_24_scope() {
    run("rl_24_scope", &["RL-24"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |mut c| {
            c.bypass_route_prefixes = Some(vec!["svc/public".into()]);
            c.throttler(|t| {
                t.global_ip_rate_limit = 1.0;
                t.anonymous_ip_rate_limit = Some(1.0);
                t.anonymous_ip_rate_limit_ttl = Some(60.0);
            })
        })
        .await?;

        let static_requests = send_all(
            &gw,
            &vec![Request::get("/svc/v1/users").header("x-test-static", "1"); 3],
        )
        .await?;
        assert_eq!(statuses(&static_requests), [200, 200, 200]);
        obs.record("Static Requests", statuses(&static_requests));

        let bypass = send_all(&gw, &vec![Request::get("/svc/public/anything"); 3]).await?;
        assert_eq!(statuses(&bypass), [200, 200, 200]);
        obs.record("Bypass Requests", statuses(&bypass));

        let no_route = send_all(&gw, &vec![Request::new("PUT", "/svc/v1/users"); 3]).await?;
        assert_eq!(statuses(&no_route), [405, 405, 405]);
        obs.record("no matching Route", statuses(&no_route));

        // The sockets stay open until the end: the reference crashes (EPIPE) when a client
        // drops a tunnelled socket it is still writing to.
        // The upgrade's upstream target is not the stripped path, so every request gets the
        // WebSocket behaviour while upgrading.
        mock.set_default(Behaviour::WebSocketEcho);
        let mut sockets = Vec::new();
        let mut upgrades = Vec::new();
        for _ in 0..3 {
            let upgrade =
                client::websocket(gw.addr(), &client::upgrade_request("/svc/socket")).await?;
            upgrades.push(upgrade.response_head.status);
            sockets.push(upgrade.socket);
        }
        assert_eq!(upgrades, [101, 101, 101]);
        obs.record("WebSocket upgrades", upgrades);
        mock.set_default(Behaviour::Echo);

        // The limiter is active: the first Routed Request opens the Bucket, the next is full.
        let routed = send_all(&gw, &vec![Request::get("/svc/v1/users"); 2]).await?;
        assert_eq!(statuses(&routed), [200, 429]);
        obs.record("Routed Requests afterwards", statuses(&routed));
        drop(gw);
        drop(sockets);
        Ok(obs)
    })
    .await;
}
