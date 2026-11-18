//! Rust-only tests of rate limiting in the served gateway: Deviation D10 (REQ-24), Redis
//! restarts (CFG-17) and the Identity resolver reading what middleware attached (REQ-20,
//! RL-13). Tests needing a real Redis use `CONFORMANCE_REDIS_PORT` and print `SKIP` without
//! one (see `rate_limit_common`).

mod gateway_common;
mod rate_limit_common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use api_gateway::gateway::{Gateway, GatewayOptions};
use api_gateway::handler::{
    BoxError, GatewayRequest, IdentityContext, IdentityResolver, MiddlewareError, ProxyMiddleware,
    async_trait,
};
use api_gateway::rate_limit::{BucketLimit, Identity, RateLimitOptions, RedisOptions, TrustProxy};
use api_gateway::request::ProxyRequest;
use api_gateway::route::Route;
use gateway_common::{Upstream, client, options, serve};
use rate_limit_common::{Blackhole, Proxy, nonce};

const INTERNAL: &str = r#"{"statusCode":500,"message":"Internal server error"}"#;

fn limited(upstream: &Upstream, redis: RedisOptions, global_ip: u64) -> GatewayOptions {
    let mut options = options(vec![upstream.service("svc")]);
    options.rate_limit = RateLimitOptions::new(
        true,
        BucketLimit::new(global_ip, 60.0),
        BucketLimit::new(1_000_000, 60.0),
    );
    options.rate_limit.trust_proxy = TrustProxy::All;
    options.redis = redis;
    options
}

/// REQ-24 under Deviation D10: when Redis cannot be reached, a request that needs a Bucket
/// answers 500 within 2 seconds, whether Redis refuses connections or never answers. The
/// reference waits about 73 s (`errors::req_24_redis_unreachable`).
#[tokio::test]
async fn d10_redis_unreachable_answers_500_within_two_seconds() {
    let upstream = Upstream::start().await;
    let blackhole = Blackhole::start().await;
    let refused = RedisOptions::new("127.0.0.1", 1);
    for (label, redis) in [
        ("unresponsive", blackhole.redis_options()),
        ("refused", refused),
    ] {
        let served = serve(Gateway::builder(limited(&upstream, redis, 1_000_000)), true).await;
        let started = Instant::now();
        let response = client()
            .get(served.url("/svc/v1/users"))
            .send()
            .await
            .unwrap();
        let waited = started.elapsed();
        assert_eq!(response.status(), 500, "{label}");
        assert_eq!(response.text().await.unwrap(), INTERNAL, "{label}");
        assert!(waited < Duration::from_secs(2), "{label}: {waited:?}");
    }
    assert!(blackhole.accepted() >= 1);
    assert!(upstream.seen().is_empty(), "nothing was forwarded");
}

/// CFG-17: rate limiting keeps working after Redis restarts: requests fail with 500 while it
/// is down (D10) and are limited again once it is back.
#[tokio::test]
async fn cfg_17_rate_limiting_survives_a_redis_restart() {
    require_redis!();
    let upstream = Upstream::start().await;
    let redis = Proxy::start().await;
    let served = serve(
        Gateway::builder(limited(&upstream, redis.redis_options(), 2)),
        true,
    )
    .await;
    let from = |client_address: &str| {
        client()
            .get(served.url("/svc/v1/users"))
            .header("x-forwarded-for", client_address.to_string())
    };
    // Any string works as a client address (non-addresses are used as they are).
    let address = format!("client-{}", nonce());
    assert_eq!(from(&address).send().await.unwrap().status(), 200);

    redis.stop().await;
    let response = from(&address).send().await.unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(response.text().await.unwrap(), INTERNAL);

    redis.restart().await;
    let mut statuses = Vec::new();
    for _ in 0..2 {
        statuses.push(from(&address).send().await.unwrap().status().as_u16());
    }
    // The Bucket kept its first count: one more request is admitted, then 429.
    assert_eq!(statuses, [200, 429]);
}

#[derive(Debug, Clone)]
struct User(String);

/// Attaches the `x-user` header's value to the request.
struct AttachUser;

#[async_trait]
impl ProxyMiddleware for AttachUser {
    async fn handle(
        &self,
        _route: &Route,
        request: &mut GatewayRequest,
        _proxy_request: &mut ProxyRequest,
    ) -> Result<bool, MiddlewareError> {
        if let Some(user) = request.header("x-user").map(str::to_string) {
            request.extensions_mut().insert(User(user));
        }
        Ok(true)
    }
}

/// What the resolver saw: the attached user, the Route's operation id, the address.
type Sighting = (Option<String>, String, String);

/// Keys requests by the attached user and records what it saw; fails for `x-fail`.
struct ByUser {
    seen: Arc<Mutex<Vec<Sighting>>>,
}

#[async_trait]
impl IdentityResolver for ByUser {
    async fn resolve(&self, context: IdentityContext<'_>) -> Result<Option<Identity>, BoxError> {
        if context.request.header("x-fail").is_some() {
            return Err("resolver failure".into());
        }
        let user = context
            .request
            .extensions()
            .get::<User>()
            .map(|u| u.0.clone());
        self.seen.lock().unwrap().push((
            user.clone(),
            context.route.operation_id.clone().unwrap_or_default(),
            context.ip.to_string(),
        ));
        Ok(user.map(Identity::authenticated))
    }
}

/// REQ-20 and RL-13: the Identity resolver runs after middleware and reads what they attached
/// to the request; it gets the Route and the normalised client address. A resolver failure
/// answers 500 (REQ-24).
#[tokio::test]
async fn req_20_rl_13_resolver_reads_what_middleware_attached() {
    require_redis!();
    let upstream = Upstream::start().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut gateway_options = limited(&upstream, rate_limit_common::redis_options(), 1_000_000);
    gateway_options.rate_limit.custom = BucketLimit::new(1, 60.0);
    let served = serve(
        Gateway::builder(gateway_options)
            .proxy_middleware(AttachUser)
            .identity_resolver(ByUser {
                seen: Arc::clone(&seen),
            }),
        true,
    )
    .await;
    let user = format!("user-{}", nonce());
    let send = |user: &str| {
        client()
            .get(served.url("/svc/v1/users"))
            .header("x-forwarded-for", "2001:db8::7")
            .header("x-user", user.to_string())
            .send()
    };
    assert_eq!(send(&user).await.unwrap().status(), 200);
    // The Custom Bucket (limit 1) is keyed by the resolved Identity.
    assert_eq!(send(&user).await.unwrap().status(), 429);
    assert_eq!(send(&format!("{user}-other")).await.unwrap().status(), 200);
    assert_eq!(
        seen.lock().unwrap()[0],
        (
            Some(user.clone()),
            "listUsers".to_string(),
            "2001:db8:0:0:0:0:0:0/64".to_string()
        )
    );

    let response = client()
        .get(served.url("/svc/v1/users"))
        .header("x-fail", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(response.text().await.unwrap(), INTERNAL);
}
