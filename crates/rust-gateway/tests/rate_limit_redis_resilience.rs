//! Redis outages: CFG-17 (recovery after a restart), Deviation D10 (bounded failure) and
//! RL-01 (a disabled limiter never connects).

mod rate_limit_common;

use std::time::{Duration, Instant};

use api_gateway::rate_limit::{
    BucketLimit, Identity, REDIS_OPERATION_TIMEOUT, RateLimitError, RateLimiter, RedisOptions,
    RouteKey, RouteLimit, StoreError,
};
use rate_limit_common::{Blackhole, Proxy, limiter_on, nonce, options, state};

async fn count(limiter: &RateLimiter, client: &str) -> Result<(), RateLimitError> {
    let mut s = state(limiter, client);
    limiter.count_global_ip(&mut s).await
}

/// The bound Deviation D10 allows for the whole request.
const D10_BOUND: Duration = Duration::from_secs(2);

#[test]
fn d10_operation_timeout_leaves_headroom() {
    assert!(REDIS_OPERATION_TIMEOUT < D10_BOUND);
}

/// CFG-17: the limiter keeps working after Redis restarts (every connection dropped, the
/// script cache emptied), and its first request after the restart already succeeds.
#[tokio::test]
async fn cfg_17_survives_a_redis_restart() {
    require_redis!();
    let proxy = Proxy::start().await;
    let mut opts = options();
    opts.global_ip = BucketLimit::new(3, 60.0);
    let l = limiter_on(&opts, &proxy.redis_options());
    let client = format!("client-{}", nonce());

    count(&l, &client).await.expect("before the restart");

    // Redis goes down: requests fail (fast: the connection is refused).
    proxy.stop().await;
    let started = Instant::now();
    let down = count(&l, &client).await;
    assert!(matches!(down, Err(RateLimitError::Store(_))), "{down:?}");
    assert!(started.elapsed() < D10_BOUND);

    // Redis comes back with an empty script cache.
    let mut raw = rate_limit_common::raw_redis().await;
    let _: () = redis::cmd("SCRIPT")
        .arg("FLUSH")
        .query_async(&mut raw)
        .await
        .expect("SCRIPT FLUSH");
    proxy.restart().await;
    count(&l, &client)
        .await
        .expect("first request after the restart");
    count(&l, &client)
        .await
        .expect("second request after the restart");

    // A restart while the connection is idle: the next request transparently reconnects.
    proxy.stop().await;
    proxy.restart().await;
    let rejected = count(&l, &client).await;
    assert!(
        matches!(rejected, Err(RateLimitError::TooManyRequests(_))),
        "counts survived (3 admitted, 4th rejected): {rejected:?}"
    );
}

/// D10: Redis refusing connections fails the operation at once.
#[tokio::test]
async fn d10_refused_connection_fails_fast() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let l = limiter_on(&options(), &RedisOptions::new("127.0.0.1", port));
    for _ in 0..3 {
        let started = Instant::now();
        let result = count(&l, "203.0.113.1").await;
        assert!(
            matches!(result, Err(RateLimitError::Store(StoreError::Connect(_)))),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}

/// D10: Redis that accepts connections but never answers fails every operation within
/// `REDIS_OPERATION_TIMEOUT`, so a request answers 500 within 2 seconds.
#[tokio::test]
async fn d10_unresponsive_redis_fails_within_the_bound() {
    let blackhole = Blackhole::start().await;
    let l = limiter_on(&options(), &blackhole.redis_options());
    let mut tasks = Vec::new();
    for i in 0..5 {
        let l = l.clone();
        tasks.push(tokio::spawn(async move {
            let started = Instant::now();
            let mut s = state(&l, &format!("203.0.113.{i}"));
            let result = l.count_global_ip(&mut s).await;
            (started.elapsed(), result)
        }));
    }
    for task in tasks {
        let (elapsed, result) = task.await.expect("task");
        assert!(
            matches!(result, Err(RateLimitError::Store(StoreError::Timeout(_)))),
            "{result:?}"
        );
        assert!(elapsed >= REDIS_OPERATION_TIMEOUT, "{elapsed:?}");
        assert!(elapsed < D10_BOUND, "{elapsed:?}");
    }
    // A whole Routed Request needing several Buckets fails at its first one.
    let started = Instant::now();
    let mut s = state(&l, "203.0.113.99");
    let result = l
        .check_limits(
            &mut s,
            Some(Identity::anonymous("someone")),
            RouteKey::new("svc", "GET", "/v1/x"),
            &[RouteLimit {
                limit: 1,
                ttl: 60.0,
                status: None,
            }],
        )
        .await;
    assert!(matches!(result, Err(RateLimitError::Store(_))));
    assert!(started.elapsed() < D10_BOUND);
}

/// RL-01: a disabled limiter never contacts Redis.
#[tokio::test]
async fn rl_01_disabled_limiter_never_connects() {
    let blackhole = Blackhole::start().await;
    let mut opts = options();
    opts.enabled = false;
    opts.anonymous_ip_limit = Some(1);
    opts.anonymous_ip_ttl = Some(60.0);
    let l = limiter_on(&opts, &blackhole.redis_options());
    let route = RouteKey::new("svc", "POST", "/v1/users");
    let limits = [RouteLimit {
        limit: 1,
        ttl: 60.0,
        status: Some(201),
    }];
    for _ in 0..3 {
        let mut s = state(&l, "203.0.113.5");
        l.count_global_ip(&mut s).await.expect("no-op");
        l.count_anonymous_ip(&mut s).await.expect("no-op");
        let identity = Some(s.default_identity());
        l.check_limits(&mut s, identity, route, &limits)
            .await
            .expect("no-op");
        l.count_after_response(&s, route, &limits, 201)
            .await
            .expect("no-op");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(blackhole.accepted(), 0);
}
