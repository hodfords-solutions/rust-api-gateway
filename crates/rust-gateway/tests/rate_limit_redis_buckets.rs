//! Buckets against a real Redis: RL-02 to RL-05, RL-16, RL-17, RL-19.

mod rate_limit_common;

use std::time::{Duration, Instant};

use rate_limit_common::{limiter, nonce, options, state};
use rust_gateway::rate_limit::{
    BucketLimit, Identity, RateLimitError, RateLimiter, RouteKey, RouteLimit, TooManyRequests,
};

fn admitted(result: &Result<(), RateLimitError>) -> bool {
    match result {
        Ok(()) => true,
        Err(RateLimitError::TooManyRequests(_)) => false,
        Err(other) => panic!("unexpected error: {other}"),
    }
}

async fn global_ip(limiter: &RateLimiter, client: &str) -> bool {
    let mut state = state(limiter, client);
    admitted(&limiter.count_global_ip(&mut state).await)
}

/// RL-02: a fixed window. The first request opens it, `limit` requests are admitted,
/// rejected requests do not extend it, and it does not slide: once it expires the next
/// request opens a new full window even though recent requests were admitted.
#[tokio::test]
async fn rl_02_fixed_window() {
    require_redis!();
    let mut opts = options();
    opts.global_ip = BucketLimit::new(2, 1.0);
    let l = limiter(&opts);
    let client = format!("client-{}", nonce());

    let start = Instant::now();
    assert!(global_ip(&l, &client).await, "opens the window");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(global_ip(&l, &client).await, "second in window");
    assert!(!global_ip(&l, &client).await, "third rejected");
    assert!(!global_ip(&l, &client).await, "still rejected");

    // Past the first window (1 s after the first request), but within 1 s of the second.
    let until = start + Duration::from_millis(1200);
    tokio::time::sleep_until(until.into()).await;
    assert!(global_ip(&l, &client).await, "new window");
    assert!(
        global_ip(&l, &client).await,
        "a sliding window would count the request at 0.5 s and reject this one"
    );
    assert!(!global_ip(&l, &client).await);
}

/// RL-03: a limit of 0 admits the request that opens the window, then rejects.
#[tokio::test]
async fn rl_03_limit_zero_behaves_like_one() {
    require_redis!();
    let mut opts = options();
    opts.global_ip = BucketLimit::new(0, 60.0);
    let l = limiter(&opts);
    let client = format!("client-{}", nonce());
    assert!(global_ip(&l, &client).await);
    assert!(!global_ip(&l, &client).await);
    assert!(!global_ip(&l, &client).await);
}

/// RL-04: fractional windows, truncated to whole milliseconds.
#[tokio::test]
async fn rl_04_fractional_window() {
    require_redis!();
    let mut opts = options();
    opts.global_ip = BucketLimit::new(1, 0.3009);
    let l = limiter(&opts);
    let client = format!("client-{}", nonce());
    assert!(global_ip(&l, &client).await);
    assert!(!global_ip(&l, &client).await);
    let mut redis = rate_limit_common::raw_redis().await;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("*{client}*"))
        .query_async(&mut redis)
        .await
        .expect("KEYS");
    assert_eq!(keys.len(), 1);
    let pttl: i64 = redis::cmd("PTTL")
        .arg(&keys[0])
        .query_async(&mut redis)
        .await
        .expect("PTTL");
    assert!((1..=300).contains(&pttl), "window is 300 ms, PTTL {pttl}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(global_ip(&l, &client).await, "expired after 300 ms");
}

/// RL-05: concurrent callers on several gateway instances (separate limiters, each with
/// its own connection) never exceed the limit.
#[tokio::test]
async fn rl_05_concurrent_instances_never_exceed_the_limit() {
    require_redis!();
    let mut opts = options();
    opts.global_ip = BucketLimit::new(50, 60.0);
    let instances: Vec<RateLimiter> = (0..4).map(|_| limiter(&opts)).collect();
    let client = format!("client-{}", nonce());
    let mut tasks = Vec::new();
    for i in 0..400 {
        let l = instances[i % instances.len()].clone();
        let client = client.clone();
        tasks.push(tokio::spawn(async move { global_ip(&l, &client).await }));
    }
    let mut admitted_count = 0;
    for task in tasks {
        if task.await.expect("task") {
            admitted_count += 1;
        }
    }
    assert_eq!(admitted_count, 50);
}

const PLAIN: RouteLimit = RouteLimit {
    limit: 2,
    ttl: 60.0,
    status: None,
};

/// RL-17 (Deviation D2): every limit of a Route has its own Bucket, and Routes with the same
/// method and pattern on different Upstream Services do not share Buckets.
#[tokio::test]
async fn rl_17_buckets_per_limit_and_service() {
    require_redis!();
    let l = limiter(&options());
    let id = format!("user-{}", nonce());
    let users = RouteKey::new("users", "GET", "/v1/limits/plain");
    let default = RouteKey::new("default", "GET", "/v1/limits/plain");

    let check = |route: RouteKey<'static>, limits: Vec<RouteLimit>| {
        let l = l.clone();
        let id = id.clone();
        async move {
            let mut s = state(&l, "198.51.100.17");
            l.check_limits(&mut s, Some(Identity::anonymous(id)), route, &limits)
                .await
        }
    };

    // Same pattern, two services: each admits its own two requests.
    for route in [users, default] {
        assert!(admitted(&check(route, vec![PLAIN]).await));
        assert!(admitted(&check(route, vec![PLAIN]).await));
    }
    assert!(matches!(
        check(users, vec![PLAIN]).await,
        Err(RateLimitError::TooManyRequests(TooManyRequests::Route { ttl })) if ttl == 60.0
    ));
    assert!(!admitted(&check(default, vec![PLAIN]).await));

    // Stacked limits: (5 per 2 h, 3 per 60 s). The reference shares one counter, so it
    // rejects the 4th request with the 2 h text after the 3rd; here each limit counts on its
    // own, and the 60 s limit is the one that fills.
    let stacked = RouteKey::new("users", "GET", "/v1/limits/stacked");
    let limits = vec![
        RouteLimit {
            limit: 5,
            ttl: 7200.0,
            status: None,
        },
        RouteLimit {
            limit: 3,
            ttl: 60.0,
            status: None,
        },
    ];
    for _ in 0..3 {
        assert!(admitted(&check(stacked, limits.clone()).await));
    }
    assert!(matches!(
        check(stacked, limits.clone()).await,
        Err(RateLimitError::TooManyRequests(TooManyRequests::Route { ttl })) if ttl == 60.0
    ));
    // The 2 h Bucket counted all four requests (it comes first), so a Route with only it
    // has one request left.
    assert!(admitted(&check(stacked, vec![limits[0]]).await));
    assert!(!admitted(&check(stacked, vec![limits[0]]).await));

    // A limit with the same numbers but a status is yet another Bucket.
    let scoped = RouteLimit {
        status: Some(201),
        ..PLAIN
    };
    assert!(admitted(&check(users, vec![scoped]).await));
}

/// RL-16, RL-19: status-scoped Buckets count after the response, with the request's
/// Identity, only for the configured status, and then reject further requests.
#[tokio::test]
async fn rl_16_status_scoped_bucket_counts_after_response() {
    require_redis!();
    let l = limiter(&options());
    let route = RouteKey::new("users", "POST", "/v1/limits/on-status");
    let limits = [RouteLimit {
        limit: 1,
        ttl: 60.0,
        status: Some(201),
    }];
    let id = format!("user-{}", nonce());
    let other = format!("other-{}", nonce());

    let mut first = state(&l, "198.51.100.16");
    l.check_limits(
        &mut first,
        Some(Identity::anonymous(id.clone())),
        route,
        &limits,
    )
    .await
    .expect("admitted");
    l.count_after_response(&first, route, &limits, 400)
        .await
        .expect("not counted");

    let mut second = state(&l, "198.51.100.16");
    l.check_limits(
        &mut second,
        Some(Identity::anonymous(id.clone())),
        route,
        &limits,
    )
    .await
    .expect("a 400 did not count");
    l.count_after_response(&second, route, &limits, 201)
        .await
        .expect("counted");

    let mut third = state(&l, "198.51.100.16");
    assert!(matches!(
        l.check_limits(&mut third, Some(Identity::anonymous(id)), route, &limits)
            .await,
        Err(RateLimitError::TooManyRequests(
            TooManyRequests::Route { .. }
        ))
    ));
    let mut fourth = state(&l, "198.51.100.16");
    l.check_limits(
        &mut fourth,
        Some(Identity::anonymous(other)),
        route,
        &limits,
    )
    .await
    .expect("another Identity has its own Bucket");
}
