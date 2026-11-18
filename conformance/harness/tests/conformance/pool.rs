//! Upstream connection pool: CFG-10, as far as it is observable.

use std::time::{Duration, Instant};

use conformance::client::Request;
use conformance::mock::{Behaviour, Reply};
use conformance::runner::{Observations, run};
use serde_json::json;

use crate::common::{error_view, users_gateway};

/// CFG-10: connection reuse, the idle keep-alive timeout, the per-service connection limit,
/// pass-through of undici options, and the absence of a short default headers timeout.
#[tokio::test]
async fn cfg_10_upstream_pool() {
    run("cfg_10_upstream_pool", &["CFG-10"], |target| async move {
        let mut obs = Observations::new();
        let (mock, gw) = users_gateway(&target, |c| c).await?;

        // Sequential requests reuse one connection.
        mock.clear_requests();
        for _ in 0..3 {
            assert_eq!(gw.get("/svc/v1/users").await?.status, 200);
        }
        let ids: Vec<u64> = mock.requests().iter().map(|r| r.connection).collect();
        let reused = ids.windows(2).all(|w| w[0] == w[1]);
        assert!(
            reused,
            "sequential requests should share a connection: {ids:?}"
        );
        obs.record("sequential requests reuse a connection", reused);

        // An idle connection is dropped after undici's default keepAliveTimeout (4 s).
        let before = mock.requests().last().unwrap().connection;
        tokio::time::sleep(Duration::from_millis(5_000)).await;
        gw.get("/svc/v1/users").await?;
        let after = mock.requests().last().unwrap().connection;
        assert_ne!(before, after, "a connection idle for 5 s is not reused");
        obs.record("idle 5 s opens a new connection", before != after);

        // No short default headers timeout: a 3 s upstream still answers.
        mock.route(
            "/v1/users/slow",
            Behaviour::Delay(
                Duration::from_secs(3),
                Box::new(Behaviour::Respond(Reply::new(200).body("late"))),
            ),
        );
        let slow = gw.get("/svc/v1/users/slow").await?;
        assert_eq!((slow.status, slow.text().as_str()), (200, "late"));
        obs.record("3 s upstream answers", slow.status);
        drop(gw);

        // `connections: 1` serialises concurrent requests on one connection.
        let (mock, gw) = users_gateway(&target, |c| {
            c.pool(json!({ "connections": 1, "pipelining": 1 }))
        })
        .await?;
        mock.route(
            "/v1/users/wait",
            Behaviour::Delay(
                Duration::from_millis(400),
                Box::new(Behaviour::Respond(Reply::new(200))),
            ),
        );
        let started = Instant::now();
        let request = Request::get("/svc/v1/users/wait");
        let (a, b, c) = tokio::join!(gw.send(&request), gw.send(&request), gw.send(&request));
        let elapsed = started.elapsed();
        for r in [a?, b?, c?] {
            assert_eq!(r.status, 200);
        }
        let ids: Vec<u64> = mock.requests().iter().map(|r| r.connection).collect();
        let one_connection = ids.windows(2).all(|w| w[0] == w[1]);
        assert!(
            one_connection,
            "connections: 1 uses one connection: {ids:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(1_150),
            "serialised: {elapsed:?}"
        );
        obs.record("connections: 1 serialises requests", one_connection);
        drop(gw);

        // Other undici options are passed through: headersTimeout ends a silent upstream.
        let (mock, gw) = users_gateway(&target, |c| {
            c.pool(json!({ "connections": 10, "headersTimeout": 500 }))
        })
        .await?;
        mock.route("/v1/users/hang", Behaviour::Hang);
        let started = Instant::now();
        let timed_out = gw.get("/svc/v1/users/hang").await?;
        let elapsed = started.elapsed();
        assert_eq!(timed_out.status, 503);
        assert_eq!(timed_out.text(), r#"{"message":"Service unavailable."}"#);
        assert!(
            elapsed < Duration::from_secs(5),
            "headersTimeout applied: {elapsed:?}"
        );
        obs.record("headersTimeout 500 ms", error_view(&timed_out));
        Ok(obs)
    })
    .await;
}
